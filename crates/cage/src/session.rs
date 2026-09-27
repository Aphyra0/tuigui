//! Session lifecycle: spawn `cage` headless with a private Wayland socket and
//! launch the target app inside it.

use std::collections::HashMap;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::CageError;

/// What to launch and how.
#[derive(Debug, Clone)]
pub struct CageSpec {
    /// Path to the application binary (resolved in PATH if not absolute).
    pub app: PathBuf,
    /// Arguments for the app.
    pub args: Vec<String>,
    /// Extra environment for the app (merged over inherited env).
    pub env: HashMap<String, String>,
    /// Optional working directory for the app.
    pub cwd: Option<PathBuf>,
}

impl CageSpec {
    pub fn new(app: impl Into<PathBuf>) -> Self {
        CageSpec {
            app: app.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
}

/// Tunables for the headless compositor.
#[derive(Debug, Clone)]
pub struct HeadlessConfig {
    /// Name of the private Wayland socket (under XDG_RUNTIME_DIR).
    /// Empty = derive a unique name from the PID.
    pub socket_name: String,
    /// Virtual output size (the app sees a screen of this size). There is no
    /// sensible default: the caller must supply the real terminal pixel size.
    /// 0 is rejected by [`CageSession::spawn`].
    pub width: u32,
    pub height: u32,
    /// Override the cage binary path (defaults to `cage` in PATH,
    /// or `TUIGUI_CAGE_BIN`).
    pub cage_bin: Option<PathBuf>,
}

#[allow(clippy::derivable_impls)] // width/height = 0 is load-bearing: spawn rejects it.
impl Default for HeadlessConfig {
    fn default() -> Self {
        HeadlessConfig {
            socket_name: String::new(),
            width: 0,
            height: 0,
            cage_bin: None,
        }
    }
}

/// A running headless session. Drop to terminate (killing cage and the app).
pub struct CageSession {
    cage: Child,
    socket_path: PathBuf,
}

impl CageSession {
    /// Spawn cage (headless) with `spec.app` as its single client. Resolves
    /// once the Wayland socket exists.
    pub async fn spawn(spec: CageSpec, cfg: HeadlessConfig) -> Result<CageSession, CageError> {
        if cfg.width == 0 || cfg.height == 0 {
            return Err(CageError::Capture(format!(
                "headless output size must be nonzero, got {}x{} (caller must supply the real terminal pixel size)",
                cfg.width, cfg.height
            )));
        }
        let cage_bin = cfg
            .cage_bin
            .clone()
            .or_else(|| std::env::var_os("TUIGUI_CAGE_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("cage"));

        // Use a private runtime dir, never the caller's. wlroots (and so cage)
        // name their socket `wayland-N` here; a private dir guarantees the only
        // `wayland-*` socket present is cage's own, so the detection below
        // cannot mistake the user's real compositor for our headless one.
        let runtime_dir = PathBuf::from(format!(
            "{}/tuigui-{}",
            std::env::temp_dir().display(),
            std::process::id()
        ));
        std::fs::create_dir_all(&runtime_dir)?;

        // wlroots (and so cage) name their socket `wayland-N` under
        // XDG_RUNTIME_DIR. If the caller pinned a socket name we honor it via
        // WAYLAND_DISPLAY (cage passes it to the app); otherwise we leave cage
        // to whatever it names its own socket and detect it below.
        let mut cage_cmd = Command::new(&cage_bin);
        cage_cmd
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_HEADLESS_OUTPUT_NAME", "TUIGUI-HEAD")
            .env("WLR_HEADLESS_OUTPUT_WIDTH", cfg.width.to_string())
            .env("WLR_HEADLESS_OUTPUT_HEIGHT", cfg.height.to_string())
            .arg("--")
            .arg(&spec.app)
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // A named Wayland socket means "give us a display with this exact
        // name"; default keeps cage's own `wayland-0`.
        if !cfg.socket_name.is_empty() {
            cage_cmd.env("WAYLAND_DISPLAY", &cfg.socket_name);
        }

        if let Some(cwd) = &spec.cwd {
            cage_cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cage_cmd.env(k, v);
        }

        let mut cage = cage_cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CageError::CageMissing,
            _ => CageError::Io(e),
        })?;

        // Wait for a `wayland-*` socket to appear in the runtime dir.
        let sock = runtime_dir.clone();
        let socket_wait = async move {
            for _ in 0..250 {
                if let Ok(entries) = std::fs::read_dir(&sock) {
                    for e in entries.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.starts_with("wayland-") {
                            if let Ok(symlink_meta) = std::fs::symlink_metadata(e.path()) {
                                if symlink_meta.file_type().is_socket() {
                                    return Ok(e.path());
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(CageError::CageDied(format!(
                "no Wayland socket appeared in {}",
                sock.display()
            )))
        };
        tokio::pin!(socket_wait);

        let socket_path: PathBuf = tokio::select! {
            r = &mut socket_wait => r?,
            status = cage.wait() => {
                let out = status.map_err(CageError::Io)?;
                return Err(CageError::CageDied(format!("exit code {:?}", out.code())));
            }
        };

        Ok(CageSession { cage, socket_path })
    }

    /// Absolute path of the private Wayland socket.
    pub fn wayland_socket(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Terminate cage (and thereby the app) and wait for exit.
    pub async fn shutdown(mut self) -> Result<(), CageError> {
        #[cfg(unix)]
        if let Some(pid) = self.cage.id() {
            kill_signal(pid, 15);
        }
        match tokio::time::timeout(std::time::Duration::from_secs(2), self.cage.wait()).await {
            Ok(status) => {
                status.map_err(CageError::Io)?;
                Ok(())
            }
            Err(_) => {
                self.cage.kill().await.ok();
                Ok(())
            }
        }
    }
}

impl Drop for CageSession {
    fn drop(&mut self) {
        // Best-effort; shutdown() is the polite path.
        if let Some(pid) = self.cage.id() {
            #[cfg(unix)]
            kill_signal(pid, 9);
        }
    }
}

// Minimal signal helpers to avoid a libc dependency: just the two we need.
#[cfg(unix)]
fn kill_signal(pid: u32, sig: i32) {
    std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .output()
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_builder() {
        let s = CageSpec::new("foot")
            .arg("--server")
            .env("SHELL", "/bin/sh");
        assert_eq!(s.args, vec!["--server"]);
        assert_eq!(s.env.get("SHELL").unwrap(), "/bin/sh");
    }
}

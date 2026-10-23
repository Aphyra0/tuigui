{
  description = "tuigui: GUI apps in the terminal — headless Wayland head + TGP streaming";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/20b1ddd1aa5ace70c9468305030aa4f9ef79671b";
    # Binary rust distribution (rustup profiles) instead of nixpkgs' from-source
    # rustc: ~1.2 GB devshell closure instead of ~2 GB.
    rust-overlay.url = "github:oxalica/rust-overlay/84fb3c79e477d5e7ec4eff9c24dbc5d5c99b0fd1";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { self, nixpkgs, rust-overlay }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          # Same binary toolchain as the dev shell (upstream dist tarballs,
          # rustc+cargo+rust-std in one prefix) wired into buildRustPackage
          # via makeRustPlatform, instead of nixpkgs' from-source rustc.
          toolchain = pkgs.rust-bin.stable.latest.minimal;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
        in
        {
          # Public-domain (1887 Eadweard Muybridge) capybara clip, transcoded
          # from Wikimedia's WebM to a YUV420P H.264 MP4 (even-dims) so any
          # consumer can decode it with baseline ffmpeg. Stable URL, no
          # attribution or reuse restrictions.
          # capyVideo — sample capybara video for the `--video` streaming path.
          #
          # Source: "Animal Locomotion Pl.746 - Capybara Walking" (1887), a
          # manually reconstructed animation of Eadweard Muybridge's walk-cycle
          # photographs (photos 1-9 of Plate 746).
          #   - Project homepage: https://commons.wikimedia.org/wiki/File:Animal_Locomotion_Pl.746_-_Capybara_Walking.webm
          #   - Full history / background article (the 1887 Animal Locomotion
          #     plate series this clip came from):
          #     https://en.wikipedia.org/wiki/Animal_Locomotion_(Muybridge)
          #   - License: Public domain (PD-old-100 + PD-Mark; no attribution,
          #     no reuse restrictions). Not used on any wiki project page —
          #     only hosted on Wikimedia Commons.
          #   - Direct file URL (stable, permanent): the fetchurl below.
          #   - Format: 1s WebM (VP9), 1693x1221, ~1.2 MB. Transcoded to
          #     H.264 MP4 here because the source has ODD pixel dimensions
          #     (1693x1221), which baseline decoders/encoders reject; the
          #     scale filter rounds to even dims (1692x1220), YUV420P, audio
          #     dropped. ~1 MB output.
          #   - Exposed to the dev shell as `CAPY_VID_PATH`.
          capyVideo = pkgs.stdenv.mkDerivation {
            pname = "capy-video";
            version = "0.1.0";
            src = pkgs.fetchurl {
              url = "https://upload.wikimedia.org/wikipedia/commons/e/e9/Animal_Locomotion_Pl.746_-_Capybara_Walking.webm";
              sha256 = "sha256-y85/twSZdOqqEmW22vlN3p8Cod5zvmbMyX+K2KT+xwg=";
            };
            nativeBuildInputs = with pkgs; [ ffmpeg ];
            buildCommand = ''
              mkdir -p $out
              ffmpeg -y -loglevel error -i $src \
                -vf "scale=trunc(iw/2)*2:trunc(ih/2)*2" \
                -c:v libx264 -pix_fmt yuv420p -an $out/capybara.mp4
            '';
          };

          # revdiff — TUI diff-review tool used by the repo's Crush skill
          # (.agents/skills/revdiff). Not packaged in this nixpkgs pin, so
          # fetch the upstream static release tarball (hashes match the
          # published checksums.txt for v1.13.0).
          revdiff =
            let
              srcs = {
                x86_64-linux = {
                  url = "https://github.com/umputun/revdiff/releases/download/v1.13.0/revdiff_1.13.0_linux_amd64.tar.gz";
                  sha256 = "sha256-bML8vcsuhlpwY5RfBzX2mRb2jMIaiGhUIXIzx5PttMc=";
                };
                aarch64-linux = {
                  url = "https://github.com/umputun/revdiff/releases/download/v1.13.0/revdiff_1.13.0_linux_arm64.tar.gz";
                  sha256 = "sha256-QEkq6zalfxDCvKGlzJX66GnXI2v7BO+32dHD2ChRr/U=";
                };
              };
            in
            pkgs.stdenv.mkDerivation {
              pname = "revdiff";
              version = "1.13.0";
              src = pkgs.fetchurl srcs.${system};
              sourceRoot = ".";
              installPhase = ''
                install -Dm755 revdiff $out/bin/revdiff
              '';
              meta = with pkgs.lib; {
                description = "Terminal UI for reviewing and annotating diffs";
                license = licenses.mit;
                platforms = platforms.linux;
                mainProgram = "revdiff";
              };
            };

          # buildRustPackage splits the build into a `cargoDeps` derivation
          # hashed only on Cargo.lock plus the app derivation itself, so
          # dependency objects stay in the store across rebuilds that only
          # touch src/.
          default = rustPlatform.buildRustPackage {
            pname = "tuigui";
            version = "0.1.0";
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                !pkgs.lib.hasInfix ".crush" path
                && path != toString ./result
                && path != toString ./target;
            };

            cargoLock.lockFile = ./Cargo.lock;
            auditable = false;

            nativeBuildInputs = with pkgs; [
              pkg-config
              wayland-scanner
            ];
            buildInputs = with pkgs; [
              wayland
              wayland-protocols
              # system libs the compositor crate (in-process smithay head)
              # links against: pixman renderer + xkbcommon keymaps
              pixman
              libxkbcommon
              # X11 apps run through this, since it is what the compositor
              # spawns to serve them; without it a session is Wayland-only.
              xwayland
              # SIMD JPEG encoding for the streaming client. Nix's build already
              # has the SIMD kernels, so it is linked via pkg-config rather than
              # compiled from source: no cmake or nasm needed.
              libjpeg_turbo
              # decoder backend for the renderer: ffmpeg-sidecar spawns the
              # ffmpeg CLI, so it must be on PATH at build/run time
              ffmpeg
            ];

            meta = with pkgs.lib; {
              description = "GUI apps in the terminal with pixel-accurate TGP streaming";
              license = licenses.mit;
              platforms = platforms.linux;
              mainProgram = "tuigui";
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          # Capybara demo video, pinned to the capyVideo package above.
          # `CAPY_VID_PATH` points at the transcoded H.264 MP4 so dev tools
          # can stream it with `tuigui --video "$CAPY_VID_PATH"` without
          # handling licensing or re-encoding themselves.
          capy = self.packages.${system}.capyVideo or (pkgs.runCommand "capy-video" {} "mkdir -p $out; touch $out/capybara.mp4; echo 'no capybara package; build default first' >&2");
        in
        {
          default = pkgs.mkShell {
            shellHook = ''
              export CAPY_VID_PATH="${capy}/capybara.mp4"
            '';
            packages = [
              # rustup "minimal" profile (rustc, cargo, rust-std) plus the two
              # dev tools; binaries come from upstream's dist tarballs.
              (pkgs.rust-bin.stable.latest.minimal.override {
                extensions = [
                  "clippy"
                  "rustfmt"
                ];
              })
              pkgs.pkg-config
              pkgs.rust-analyzer
              # `just`, for the recipes in the justfile: building and running the
              # three binaries is a fixed set of invocations, and a shell script
              # would be the same thing with no documentation.
              pkgs.just
              pkgs.wayland
              pkgs.wayland-protocols
              pkgs.wayland-scanner
              # libs the compositor crate (in-process smithay head) links against
              pkgs.pixman
              pkgs.libxkbcommon
              # X11 apps: the compositor spawns this to serve them, so an X11-only
              # app (xeyes, glxgears) can be run in a session.
              pkgs.xwayland
              # SIMD JPEG encoding: linked against Nix's libjpeg-turbo, so it
              # only needs to be on the linker/pkg-config path.
              pkgs.libjpeg_turbo
              # demo apps to stream + TGP test terminals
              pkgs.foot
              pkgs.kitty
              # wayland-native demo clients for streaming tests. Note that the
              # EGL ones (weston-flower, weston-terminal) render through
              # dma-buf, which the compositor's shm-only capture cannot read, so
              # they capture black; the shm clients and `foot` do not.
              pkgs.weston
              # video decode for the `--video` streaming path (bypasses wayland)
              pkgs.ffmpeg
              # X11 side: xeyes + glxgears, plus a virtual X server and input
              # driver for the Xvfb capture path
              pkgs.xeyes
              pkgs.mesa-demos         # glxgears — classic X11/GL gears test app
              pkgs.xvfb-run
              pkgs.xdotool
              # profiler GUI the client half streams its spans to
              pkgs.tracy
              # diff-review TUI invoked by .agents/skills/revdiff (binary
              # release, defined in packages above)
              self.packages.${system}.revdiff
            ];
          };
        }
      );
    };
}

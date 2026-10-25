//! Forwarding bytes between this terminal and the command's pty.

use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::sync::{Arc, RwLock, Weak};
use std::thread;

use cell_map::{Cell, CellMap};
use wire::message::SurfaceId;

/// One end of the proxy: the standard streams of the process at that end.
///
/// `stdin` is what feeds that process, so the proxy writes to it; `stdout` is
/// what that process produces, so the proxy reads it. They are that process's
/// descriptors, not `tui`'s, which is why the terminal end is built with them
/// crossed: its stdin is the keyboard, so it is the field the *other* direction
/// reads from.
pub struct Streams {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Write + Send>,
}

impl Streams {
    pub fn new(
        stdin: impl Write + Send + 'static,
        stdout: impl Read + Send + 'static,
        stderr: impl Write + Send + 'static,
    ) -> Self {
        Self {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
        }
    }
}

/// Forwards bytes between two ends.
///
/// Both directions are one end's stdout into the other's stdin:
///
/// ```text
///   source.stdout  ->  dest.stdin     the command's output, to the screen
///   dest.stdout    ->  source.stdin   the keyboard, to the command
/// ```
///
/// `stderr` is written only by the end that reports a failure, so a command's
/// own stderr is never fed back into it as input.
pub struct Proxy {
    /// The command's end: the pty master.
    source: Streams,
    /// This terminal's end.
    dest: Streams,
    /// Called with every chunk of the command's output, before it is forwarded.
    ///
    /// This is the only place in the process that sees the command's bytes in
    /// order, which is why the announcement scan lives here rather than in a
    /// second reader: a second reader would split the stream, and a byte one of
    /// them consumed is a byte the other never sees.
    ///
    /// `None` for a caller that does not scan, so a session that does not need
    /// it pays nothing.
    watch: Option<Box<dyn FnMut(&[u8]) + Send>>,
    /// The screen the command is drawing into, shared with the server.
    ///
    /// The parser rather than the `Screen`, because the screen is a view of the
    /// parser's grid: sharing the parser is what lets the server read the grid
    /// without a copy per chunk. `screen()` is how it is read.
    screen: Arc<RwLock<vt100::Parser>>,
    /// The surfaces whose cells are blanked on their way to the terminal.
    ///
    /// Weak, so this never keeps a window alive: a closed window's entry stops
    /// resolving, and its cells stop being blanked.
    surfaces: Arc<RwLock<Vec<Weak<SurfaceId>>>>,
    /// The switches this proxy was started with, shared with the server.
    ///
    /// Held rather than copied: the same switches decide what the server draws and
    /// what the proxy masks, and two copies would be two answers to one question.
    debug: std::sync::Arc<crate::DebugOptions>,
}

impl Proxy {
    pub fn new(
        source: Streams,
        dest: Streams,
        screen: Arc<RwLock<vt100::Parser>>,
        surfaces: Arc<RwLock<Vec<Weak<SurfaceId>>>>,
        debug: std::sync::Arc<crate::DebugOptions>,
    ) -> Self {
        Self {
            source,
            dest,
            watch: None,
            screen,
            surfaces,
            debug,
        }
    }

    /// Inspect every chunk of the command's output as it passes.
    ///
    /// The callback runs on the forwarding thread, on the path of every byte the
    /// command writes, so it must not block. It cannot alter the bytes: it is
    /// handed a slice, never the buffer.
    pub fn watching(mut self, watch: impl FnMut(&[u8]) + Send + 'static) -> Self {
        self.watch = Some(Box::new(watch));
        self
    }

    /// Forward both directions until the command's output ends. Blocks.
    ///
    /// The output direction is the lifetime: when it ends the command is gone.
    /// The keyboard direction only makes sense while it runs, so it gets a
    /// thread, and its error is returned from here once that thread is joined.
    ///
    /// Every chunk does two things on its way past: it is fed to the shared screen,
    /// and it is rewritten so the cells of a registered surface reach the terminal
    /// blank. Both sides read the same bytes, so the screen and the terminal can
    /// never disagree about what the command drew.
    ///
    /// TODO: the split is a publisher fanning out to subscribers, which copies each
    /// chunk once per subscriber and hands out `mpsc` receivers rather than
    /// `Read`s. The better shape is one pump and two pipe handles: a single read,
    /// two writes, each consumer owning a real `BufReader` and no lock on the path.
    /// Linux could even skip the userspace copy with `tee(2)`; macOS has no
    /// equivalent, which is why the portable version is the pump.
    pub fn run(self) -> io::Result<()> {
        let Proxy {
            mut source,
            mut dest,
            mut watch,
            screen,
            surfaces,
            debug,
        } = self;

        // Named, not `thread::spawn`: an unnamed thread is an unnamed track in
        // the profiler, and a track with no name is not one that can be found.
        let keyboard = thread::Builder::new()
            .name("proxy.keyboard".into())
            .spawn(move || {
                // The zone is the subsystem: the keyboard half of the proxy.
                let _zone = tracing::info_span!("proxy.keyboard").entered();
                copy(&mut *dest.stdout, &mut *source.stdin)
            })
            .expect("the keyboard thread starts");

        let shown = copy_rewritten(
            &mut *source.stdout,
            &mut *dest.stdin,
            watch.take(),
            &mut CellMap::new(),
            &screen,
            &surfaces,
            debug.pink_placeholders,
        );
        // A panic in the keyboard thread is not a forwarding failure, so it is
        // not reported as one.
        shown.and(keyboard.join().unwrap_or(Ok(())))
    }
}

/// Copy from `from` to `to` until it ends, flushing after each read.
///
/// `EINTR` is retried rather than reported: signals are routine here, and
/// treating one as an end would stop forwarding for the rest of the session.
///
/// `?Sized` because the ends are boxed: `&mut impl Read` alone is `Sized`, and
/// the streams here are `dyn`.
fn copy<R: Read + ?Sized, W: Write + ?Sized>(from: &mut R, to: &mut W) -> io::Result<()> {
    copy_watched(from, to, None)
}

/// [`copy`], with an optional per-chunk observer.
///
/// The observer sees the same bytes that are written, in the same order, and
/// cannot alter them: it takes a slice, not a buffer. That is what makes it safe
/// to run on the hot path, and it is the property the announcement scan depends
/// on (it may only *notice* a sequence, never consume it).
///
/// Takes the observer by value so the borrow ends with the call: the caller has
/// nothing else to do with it once forwarding starts.
fn copy_watched<R: Read + ?Sized, W: Write + ?Sized>(
    from: &mut R,
    to: &mut W,
    mut watch: Option<Box<dyn FnMut(&[u8]) + Send>>,
) -> io::Result<()> {
    let mut buf = [0u8; 16384];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if let Some(watch) = watch.as_mut() {
                    watch(&buf[..n]);
                }
                to.write_all(&buf[..n])?;
                to.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// [`copy_watched`], with the cell rewrite between the read and the write.
///
/// The original bytes are what the observer sees: the announcement scan reads what
/// the command wrote, not what this side turned it into. What is written is the
/// same bytes with the cells of a registered surface blanked, which is the whole
/// point — a cell carrying a surface's signature would be drawn over the image
/// that signature anchors.
///
/// The read is a `BufReader`'s own buffer, borrowed and handed back with
/// `consume`: the chunk is never copied out and never sized by guesswork.
///
/// The chunk is parsed **once**, inside the rewrite: the same pass that feeds the
/// screen is what asks it where the character landed. Parsing it here as well
/// would advance the cursor twice per byte and leave every later cell one position
/// wrong.
#[tracing::instrument(name = "proxy.forward", skip_all)]
fn copy_rewritten<R: Read + ?Sized, W: Write + ?Sized>(
    from: &mut R,
    to: &mut W,
    mut watch: Option<Box<dyn FnMut(&[u8]) + Send>>,
    cells: &mut CellMap,
    screen: &Arc<RwLock<vt100::Parser>>,
    surfaces: &Arc<RwLock<Vec<Weak<SurfaceId>>>>,
    pink: bool,
) -> io::Result<()> {
    let mut from = BufReader::new(from);
    loop {
        let chunk = match from.fill_buf() {
            Ok([]) => return Ok(()),
            Ok(chunk) => chunk,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        // Taken now, because the borrow of the buffer ends with this call: the
        // rewriter needs the length to consume, and the work below needs the slice.
        let len = chunk.len();

        if let Some(watch) = watch.as_mut() {
            watch(chunk);
        }

        // The live surfaces, upgraded once per chunk rather than per cell. A poisoned
        // lock still holds a valid list.
        let registered = surfaces.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let live: Vec<SurfaceId> = registered
            .iter()
            .filter_map(Weak::upgrade)
            .map(|surface| *surface)
            .collect();
        drop(registered);

        let mut parser = screen.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let rewritten = cells.map(&mut parser, chunk, &mut |cell: Cell| {
            // A cell of a registered surface is blanked, and blanked on the *default*
            // background. Keeping the surface's own bg would paint over the picture:
            // the image is drawn behind the cell backgrounds, so a cell with a
            // background covers it. That is the whole reason for the mask.
            //
            // The pink marker is the debug view: a visible cell everywhere the mask
            // fired, so "the mask never matched" and "the image never arrived" are
            // different symptoms instead of the same blank pane.
            if live.iter().any(|surface| surface.matches(&cell)) {
                Cell {
                    symbol: if pink { '*' } else { ' ' },
                    fg: if pink {
                        vt100::Color::Rgb(0xff, 0x00, 0xff)
                    } else {
                        cell.fg
                    },
                    bg: vt100::Color::Default,
                }
            } else {
                cell
            }
        });
        drop(parser);

        from.consume(len);
        to.write_all(&rewritten)?;
        to.flush()?;
    }
}

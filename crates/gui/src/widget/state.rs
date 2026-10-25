//! Where the session is, and the steps that move it along.
//!
//! ```text
//!   ConnSelect ──click──▶ Connecting ──dial──▶ StartingApp ──started──▶ Streaming
//!                                                          the program ending ──▶ Done
//! ```
//!
//! Progress is made by drawing. The steps are async because the link is, and a widget's
//! `render` is not, so the async work for the current state is driven from the draw and
//! blocked on: the draw of one frame *is* one step of the session, and it takes the
//! buffer so a step that has something to say draws it where it stands.
//!
//! A step consumes the state it is given and returns the one it moved to, so the session
//! holds a [`Result`] rather than a state: what is in there is either where the session is
//! or what went wrong, and the same code draws both.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, StatefulWidget, Widget as _};
use wire::message::Message;
use wire::transport::{BoxFuture, Connection, Transport};
use wire::unix::UnixSocket;

use crate::compositor::Compositor;

use super::{socket_path, Session, CONNECTIONS};

/// How long a quiet session is paused before the window is looked at again.
///
/// A frame with nothing to send and nothing to read would otherwise return at once and be
/// drawn again immediately, so an idle program would cost a core. This is the pause that
/// keeps it cheap: the window is still looked at often enough to feel instant, and the
/// loop is not free-running.
const QUIET: std::time::Duration = std::time::Duration::from_millis(16);

/// How long a frame is given to leave this side.
///
/// A frame is a write to another process, and that process may be busy applying the
/// previous one. Bounded so a link that has stopped draining is reported rather than
/// waited on: without it the session stalls here with the frame in hand and says nothing,
/// which is a pane that looks frozen for no stated reason.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Where the session is.
///
/// Each variant carries what that step needs and nothing else, so a state cannot hold a
/// link it has not established: `StartingApp` has the [`Connection`] because dialing is
/// what produced it, and `Connecting` has the transport because it has not produced one
/// yet.
pub enum State {
    /// Nothing chosen yet: pick a socket.
    ///
    /// Carries nothing, not even a highlight: a click moves straight on, so there is no
    /// selection to remember between one draw and the next.
    ConnSelect,
    /// Announced, and waiting to be dialed.
    ///
    /// Holds the advert to draw and the wait, and nothing about what the wait is on: the
    /// future owns whatever the transport needs, so a state that only knows how to draw an
    /// address and poll a future works for any transport that can be dialed.
    ///
    /// The wait is **polled**, not awaited. `Terminal::draw` runs the drawing closure and
    /// then flushes the buffer to the terminal, so awaiting here would hold the frame open
    /// with the advert still in memory — and the dial that advert is supposed to cause
    /// could never come. Polling lets every frame finish, so the address is on the pane
    /// while the client waits.
    Connecting {
        /// The address, drawn every frame this state is in.
        advert: String,
        /// The wait, polled once per frame.
        waiting: BoxFuture<'static, std::io::Result<Connection>>,
    },
    /// The link is up, and the program is being started.
    ///
    /// The start is polled rather than awaited, for the same reason the dial is: a step
    /// that blocked here would hold the frame it is part of open, so nothing it drew would
    /// reach the terminal until it finished. The future owns the compositor and the program.
    StartingApp {
        /// The link, waiting for the compositor to start.
        connection: Connection,
        /// The start, polled once per frame.
        starting: BoxFuture<'static, std::io::Result<Compositor>>,
    },
    /// The compositor's windows are being captured and sent, through the link that is
    /// up.
    Streaming {
        connection: Connection,
        compositor: Compositor,
        /// The input read this frame, started on the previous one.
        ///
        /// A capture is one reading of the terminal's reader: the reader runs for
        /// the whole session, and finishing a capture returns what it collected
        /// since the last one. Keeping it across a frame is what lets the reading
        /// happen while the window is being encoded.
        capture: termputs::Capture,
    },
    /// The session is over. Not a failure: it is how a session is meant to finish.
    ///
    /// A state and not a return, so the one thing left to draw — that it ended — is drawn
    /// by the same match as everything else, and the loop has something to ask about.
    Done,
}

impl State {
    /// One frame: advance the session, and draw where it got to.
    ///
    /// Consumes `self` and returns the state it moved to, so a step that awaits owns what it
    /// is working on and nothing has to be copied to keep two views of it in step.
    ///
    /// A failure is handed back rather than drawn: the pane is the widget's, and a step
    /// that drew its own failures would be drawing in a place it does not own.
    #[tracing::instrument(name = "session.render", skip_all)]
    pub async fn render(
        self,
        content: Rect,
        buf: &mut Buffer,
        session: &mut Session,
    ) -> Result<State, String> {
        match self {
            // The choice, and then straight past it.
            //
            // This state exists for the list of connection types, and the list is drawn for
            // the frame it is in. Nothing waits for a click: with one transport built there
            // is nothing to choose, so drawing the options and moving on is the honest
            // version of a menu that has one entry.
            State::ConnSelect => {
                buf.set_style(content, Style::new().bg(Color::Rgb(0x18, 0x18, 0x18)));
                buf.set_string(
                    content.x + 1,
                    content.y + 1,
                    "choose the connection type",
                    Style::new().fg(Color::White),
                );
                for (index, name) in CONNECTIONS.iter().enumerate() {
                    buf.set_string(
                        content.x + 1,
                        content.y + 2 + index as u16,
                        format!(" {name} "),
                        Style::new().fg(Color::Gray),
                    );
                }

                // The socket is made and **bound** here, while it is still a `UnixSocket`:
                // binding is a socket's own business and not a transport's, and once it is
                // boxed as a transport there is nothing left to call it on. Bound before
                // anything announces it, because a peer dials the address, and the address
                // of a socket nobody bound does not answer.
                let socket = UnixSocket::new(socket_path());
                socket
                    .bind()
                    .await
                    .map_err(|e| format!("could not bind {}: {e}", socket.path().display()))?;

                // The address, and the wait. The wait owns the socket — moved into the
                // future rather than borrowed from the state — so that what is stored is a
                // future and a string, and nothing in the state has to know what kind of
                // transport it is waiting on.
                let advert = socket
                    .to_magic()
                    .ok_or_else(|| "that transport has no address to announce".to_string())?;
                log::info!("advertising {advert}");
                let waiting = Box::pin(async move { socket.accept().await });
                Ok(State::Connecting { advert, waiting })
            }

            // Listen, announce, and wait.
            //
            // This side **listens**, and that is the whole design: it is the half that
            // cannot be reached — it may be inside a multiplexer's pane, on another
            // machine, or started by something whose environment nobody controls. Its own
            // output is the only channel it is guaranteed to have, so it binds a socket of
            // its own choosing and writes that choice into the pane. The server, already
            // reading every byte the pty forwards, reads the address and dials it.
            //
            // ```text
            //   bind(path) ──▶ draw the advert into the pane ──▶ pty ──▶ server dials it
            //                                                                  │
            //                            accept ◀──────────────────────────────┘
            // ```
            //
            // Nothing is provisioned from outside: there is no session file, no path to
            // keep in step, and nothing to clean up after a crash. A stale file is a client
            // dialing a socket nobody is listening on, and announcing in-band cannot go
            // stale.
            State::Connecting {
                advert,
                mut waiting,
            } => {
                // Drawn every frame this state is in: this is the frame that is on screen
                // while the client waits, and a pane that showed nothing would look like a
                // crash.
                render_centered(content, buf, advert.clone());

                // One poll, and no waiting: the frame has to finish for this drawing to
                // reach the terminal, and the dial that this advert is supposed to cause
                // only starts once it has. A poll that is pending is the normal case for
                // every frame until the server reads the address.
                match futures::poll!(waiting.as_mut()) {
                    std::task::Poll::Ready(Ok(connection)) => {
                        log::info!("dialed");
                        // The codec is announced before anything else, so the server
                        // builds its decoder before the first frame of one arrives.
                        // Bounded like every send: a link that will not take the
                        // announcement is reported rather than waited on.
                        tokio::time::timeout(
                            SEND_TIMEOUT,
                            connection.send(&Message::InitStreaming { codec: session.codec }),
                        )
                        .await
                        .map_err(|_| format!("the link did not take the codec within {SEND_TIMEOUT:?}"))?
                        .map_err(|e| format!("the link failed: {e}"))?;
                        // The program's start is begun now and polled from the next frame,
                        // so the frame that says "starting" is flushed before anything
                        // waits on the program.
                        log::info!(
                            "starting {} at {:?}",
                            session.program.display(),
                            session.size
                        );
                        // Built here rather than inside the future: the encoder is a
                        // codec handle that is not `Send`, and a future the runtime
                        // moves between workers must be. A `Result` is what the state
                        // carries, so a failure is drawn like any other.
                        //
                        // The pane in cells comes from the output in pixels and the
                        // terminal's cell size, which are the two numbers the session
                        // already holds: a window is laid out in cells, and its picture
                        // is that box scaled to pixels.
                        let (cell_w, cell_h) = session.cell;
                        let pane = Rect::new(
                            0,
                            0,
                            (session.size.0 / cell_w.max(1)) as u16,
                            (session.size.1 / cell_h.max(1)) as u16,
                        );
                        let mut compositor = Compositor::new(session.size, pane, session.codec)
                            .map_err(|e| format!("could not start a compositor: {e}"))?;
                        compositor.spawn(&session.program).map_err(|e| {
                            format!("could not start {}: {e}", session.program.display())
                        })?;
                        Ok(State::Streaming {
                            connection,
                            compositor,
                            capture: session.input_listener.capture(),
                        })
                    }
                    std::task::Poll::Ready(Err(e)) => Err(format!("could not be dialed: {e}")),
                    std::task::Poll::Pending => Ok(State::Connecting { advert, waiting }),
                }
            }

            // Start the program in a compositor of our own.
            State::StartingApp {
                connection,
                mut starting,
            } => {
                render_centered(content, buf, "starting the program...".to_string());
                match futures::poll!(starting.as_mut()) {
                    std::task::Poll::Ready(Ok(compositor)) => {
                        let capture = session.input_listener.capture();
                        Ok(State::Streaming {
                            connection,
                            compositor,
                            capture,
                        })
                    }
                    std::task::Poll::Ready(Err(e)) => Err(e.to_string()),
                    std::task::Poll::Pending => Ok(State::StartingApp {
                        connection,
                        starting,
                    }),
                }
            }

            // One turn of the compositor, then whatever it produced goes out.
            //
            // The windows are drawn by the compositor widget itself, which is what fills
            // each window's cells with its surface: those cells are how the other side
            // finds where a picture goes, so the draw is not decoration, it is the
            // placement.
            State::Streaming {
                mut connection,
                mut compositor,
                capture,
            } => {
                // The input that arrived since the last frame. The reader is still
                // running, so this is a reading and not a hand-off: the next capture gets
                // whatever arrives after this one.
                let events = capture.finish();
                let capture = session.input_listener.capture();
                for event in &events {
                    compositor
                        .input(event, session.cell)
                        .map_err(|e| format!("could not deliver input: {e}"))?;
                }

                // One turn: step the compositor, capture every window, send what changed.
                // Bounded by the compositor's own step, and by the caller's frame.
                compositor
                    .frame(&connection)
                    .await
                    .map_err(|e| format!("the compositor failed: {e}"))?;

                // And then draw it. The draw is not decoration: each window's content
                // area is filled with that window's surface, and those cells are what
                // the other side scans for to find where a picture goes. A frame that
                // did not draw is a frame with nothing to place.
                StatefulWidget::render(&mut compositor, content, buf, &mut connection);

                // The programs are the session: when the last one has exited there is
                // nothing left to send, and nothing left for the pane to be.
                if compositor.finished() {
                    log::info!("every program has ended");
                    return Ok(State::Done);
                }

                Ok(State::Streaming {
                    connection,
                    compositor,
                    capture,
                })
            }

            // Over. Drawn so the pane says so rather than going blank, which would look
            // like a failure with nothing to report.
            State::Done => {
                render_centered(content, buf, "the program ended".to_string());
                Ok(State::Done)
            }
        }
    }

    /// A click at `(column, row)`, in the pane's own coordinates.
    ///
    /// Fails for an entry that is only named: the list offers four transports and one is
    /// built, so picking another is a failure and not a transition.
    /// Whether the session is over.
    pub fn done(&self) -> bool {
        matches!(self, State::Done)
    }
}

/// One line in the middle of `content`, for the states that are only waiting.
///
/// `Paragraph`'s own `centered` is horizontal only: a paragraph starts at the top of its
/// area, so a one-line message would sit on the first row. The middle is what is wanted
/// here, and the row is worked out rather than left to the widget.
///
/// The row is given a background of its own, and that is not decoration. What is drawn here
/// is an address that a scanner on the other side has to read off the pane, and a renderer
/// writes only the cells it *changed*: with the same background as the rest of the pane, the
/// spaces inside the message would already be spaces, so they would not be written and the
/// address would reach the far side in pieces with cursor moves between them. A background
/// of its own makes the whole row change in the frame it appears, so it is written in one
/// run — and then nothing changes, so no later frame spends bytes on it.
fn render_centered(content: Rect, buf: &mut Buffer, text: String) {
    let row = content.y + content.height.saturating_sub(1) / 2;
    let middle = Rect {
        y: row,
        height: 1.min(content.height),
        ..content
    };
    buf.set_style(content, Style::new().bg(Color::Rgb(0x18, 0x18, 0x18)));

    // The background covers the message and nothing else -- not the whole row.
    //
    // It exists because of how a renderer writes only the cells it *changed*.
    // The spaces inside the message would be spaces already, so they would not
    // be written, and the address would reach the far side in pieces with cursor
    // moves between them. Giving those cells a background of their own makes the
    // message change in the frame it appears, so it is written in one run.
    //
    // Narrow, and not the full row, because a row-wide background is a *whole
    // line* the terminal has to redraw -- and a multiplexer relaying this pane
    // sees every one of those cells as changed. The message needs its own cells
    // to be emitted together; it does not need the rest of the row rewritten.
    let line = Line::from(text);
    let width = line.width() as u16;
    // One cell right of where `Paragraph::centered` puts the text: centering
    // truncates the padding, and the two do not land on the same cell, which
    // leaves the last glyph outside the background.
    let start = middle.x + middle.width.saturating_sub(width) / 2 + 1;
    let text_area = Rect {
        x: start,
        y: row,
        width: width.min(middle.width),
        height: 1.min(content.height),
    };
    buf.set_style(text_area, Style::new().bg(Color::Rgb(0x28, 0x28, 0x28)));
    Paragraph::new(line).centered().render(middle, buf);
}

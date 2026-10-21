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
use ratatui::widgets::{Paragraph, Widget as _};
use wire::message::Message;
use wire::transport::{BoxFuture, Connection, Transport};
use wire::unix::UnixSocket;

use crate::compositor::App;

use super::{socket_path, Session, CONNECTIONS, FILL};

/// How long a quiet link is waited on before the window is looked at again.
///
/// The frame source is a poll and the link is an await, so something has to give the loop
/// a chance to poll. This is that: a link with nothing to say does not hold the session,
/// and a window that repaints while the link is quiet is still picked up.
const QUIET: std::time::Duration = std::time::Duration::from_millis(16);

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
        /// The link, waiting for the program to draw.
        connection: Connection,
        /// The start, polled once per frame.
        starting: BoxFuture<'static, std::io::Result<App>>,
    },
    /// The program's window is being captured and sent, through the link that is up.
    Streaming {
        connection: Connection,
        app: App,
        /// The input read this frame, started on the previous one.
        ///
        /// A capture is one round of reading: it exists while a thread appends
        /// to it, and finishing it returns everything that arrived. Keeping it
        /// across a frame is what makes the read overlap with the frame -- the
        /// terminal is being read while the window is being encoded -- and it is
        /// started again immediately after being finished so the next frame has
        /// one too.
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
                        // The program's start is begun now and polled from the next frame,
                        // so the frame that says "starting" is flushed before anything
                        // waits on the program.
                        log::info!(
                            "starting {} at {:?}",
                            session.program.display(),
                            session.size
                        );
                        let starting = App::start(&session.program, session.size)
                            .map_err(|e| {
                                format!("could not start {}: {e}", session.program.display())
                            })?;
                        Ok(State::StartingApp {
                            connection,
                            starting,
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
                    std::task::Poll::Ready(Ok(app)) => {
                        // The first capture starts here, so the terminal is already
                        // being read by the time the first frame is drawn.
                        let capture = session
                            .input_listener
                            .capture()
                            .map_err(|e| format!("could not read the terminal: {e}"))?;
                        Ok(State::Streaming {
                            connection,
                            app,
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

            // One frame out, or one message in: a link that has gone quiet is still
            // noticed, and the window is looked at again rather than waited on.
            State::Streaming {
                connection,
                mut app,
                capture,
            } => {
                // The picture's area, black. This is the first colour chosen for it, and
                // every pixel the terminal draws for the image goes over this.
                buf.set_style(content, Style::new().bg(Color::Black));
                // Filled with the anchor the server replaces with the image, so the
                // picture has somewhere to be and the terminal is not redrawn per frame.
                for y in content.y..content.bottom() {
                    for x in content.x..content.right() {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_char(FILL);
                        }
                    }
                }

                // The program is the session: when it ends there is nothing left to send,
                // and nothing left for the pane to be.
                if !app.running() {
                    log::info!("the program ended");
                    return Ok(State::Done);
                }

                // The input this frame, then the next capture started straight
                // away so the terminal is being read while this frame is encoded.
                //
                // Finishing joins the reader, so nothing is still holding the
                // input; starting the next one before any of the work below is
                // what keeps a keystroke from having to wait for a frame.
                let events = capture.finish();
                let capture = session
                    .input_listener
                    .capture()
                    .map_err(|e| format!("could not read the terminal: {e}"))?;
                for event in &events {
                    if let termputs::InputEvent::Key { key, state } = event {
                        app.key(key, *state)
                            .map_err(|e| format!("could not deliver a key: {e}"))?;
                    }
                }
                // A frame if the window has one. Its send is what the frame is for, so it
                // happens here rather than being handed out.
                if let Some(jpeg) = app.frame() {
                    connection
                        .send(&Message::Frame(jpeg))
                        .await
                        .map_err(|e| format!("the link failed: {e}"))?;
                    return Ok(State::Streaming {
                        connection,
                        app,
                        capture,
                    });
                }

                // Nothing to send, so the link gets the turn. A bounded wait, because a
                // quiet link must not stop the window from being polled.
                match tokio::time::timeout(QUIET, connection.recv()).await {
                    Ok(Ok(Some(Message::Error(text)))) => {
                        return Err(format!("the server failed: {text}"))
                    }
                    Ok(Ok(Some(_))) => {}
                    // The server closed. The session is over, which is not a failure.
                    Ok(Ok(None)) => return Ok(State::Done),
                    Ok(Err(e)) => return Err(format!("the link failed: {e}")),
                    // Nothing arrived within the window. Not an error: the next draw
                    // looks at the window again.
                    Err(_) => {}
                }
                Ok(State::Streaming {
                    connection,
                    app,
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
    buf.set_style(middle, Style::new().bg(Color::Rgb(0x28, 0x28, 0x28)));
    Paragraph::new(Line::from(text)).centered().render(middle, buf);
}

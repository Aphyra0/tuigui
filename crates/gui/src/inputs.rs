//! The pane's input, read and parsed.
//!
//! Reading the raw pane blocks until the pane has something to say, and the
//! draw loop cannot sit in that read, so the reading and the parsing live on a
//! thread of their own ([`start`]) and the finished events leave through a
//! channel. Bytes go to termina's parser the moment a read brings them — a
//! sequence split across bytes is the parser's problem, not the reader's.

use std::io::{BufReader, Read};
use std::sync::mpsc;

/// Start reading the pane's input, and receive what it parses.
///
/// `stdin` is the pane's input as the caller handed it in. The reader stops
/// when the pane reaches EOF or the receive end is dropped.
pub fn start(
    stdin: BufReader<Box<dyn Read + Send>>,
) -> std::io::Result<mpsc::Receiver<termina::Event>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("gui input".into())
        .spawn(move || read_inputs(stdin, sender))?;
    Ok(receiver)
}

/// Read the pane's input bytes and hand finished events to the session.
fn read_inputs(stdin: BufReader<Box<dyn Read + Send>>, sender: mpsc::Sender<termina::Event>) {
    let mut parser = termina::Parser::default();
    for byte in stdin.bytes() {
        parser.parse(&[byte.unwrap()], true);
        while let Some(event) = parser.pop() {
            sender.send(event).unwrap();
        }
    }
}

//! A terminal stream whose cells a callback may replace on the way through.
//!
//! ```text
//!   bytes ──▶ parse ──▶ Cell { symbol, fg, bg } ──▶ callback ──▶ bytes
//! ```
//!
//! A cell is the three facts that decide how it looks and nothing else. The
//! callback is asked about each one as the stream draws it and answers with what
//! should be drawn instead; answering with the cell it was given means "draw it as
//! written", which is passed through untouched.
//!
//! # What is not touched
//!
//! Every escape, control sequence and mode set is copied through exactly as it
//! arrived. Only text is rewritten, and only where the callback answered with a
//! different cell. A sequence is never split, and never re-encoded.

use vt100::Color;

/// A colour, as a cell carries it and as a caller describes one.
///
/// Re-exported so a caller never has to name the engine that supplied it.
pub use vt100::Color as CellColor;

/// One cell, as a callback sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The character drawn in it.
    pub symbol: char,
    /// The foreground it is drawn in.
    pub fg: Color,
    /// The background behind it.
    pub bg: Color,
}

/// Asked about each cell the stream draws, and answers with what to draw instead.
///
/// A function rather than a type alias with a lifetime, so a caller may hand in a
/// closure that borrows: the borrow lasts for the call and no longer.
pub trait Map {
    fn map(&mut self, cell: Cell) -> Cell;
}

impl<F: FnMut(Cell) -> Cell> Map for F {
    fn map(&mut self, cell: Cell) -> Cell {
        self(cell)
    }
}

/// A stream rewriter: it holds the bytes a read cut in half, and nothing else.
///
/// The terminal it reads is the caller's, not this type's: the same parser serves
/// whoever else needs to know what is on screen, and two parsers would be two
/// answers to one question.
#[derive(Default)]
pub struct CellMap {
    /// The start of a character or sequence that the last read cut in half. It is
    /// held until the bytes that finish it arrive, because writing half of one
    /// would put the terminal mid-character.
    carry: Vec<u8>,
}

impl CellMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `bytes` into `terminal` and return what to write to the screen instead.
    ///
    /// The terminal is fed as the bytes are walked, so the colours it reports are
    /// the ones in effect at that exact point in the stream: a rewrite never has to
    /// interpret an SGR sequence itself.
    pub fn map(
        &mut self,
        terminal: &mut vt100::Parser,
        bytes: &[u8],
        map: &mut impl Map,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + self.carry.len());
        // Anything held from the last call is the beginning of this one, and is
        // only walked once it has been joined by the bytes that complete it.
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(bytes);
        let mut rest = input.as_slice();

        while !rest.is_empty() {
            // An escape owns everything up to its final byte, and goes through in
            // one piece so a rewrite can never land inside a sequence.
            if rest[0] == 0x1b {
                let (escape, tail) = split_escape(rest);
                terminal.process(escape);
                out.extend_from_slice(escape);
                rest = tail;
                continue;
            }

            let len = utf8_len(rest[0]);
            let Some(chunk) = rest.get(..len) else {
                // A character cut off by the end of the read. It waits for the bytes
                // that complete it rather than being written half-decoded.
                self.carry.extend_from_slice(rest);
                break;
            };
            let Some(ch) = std::str::from_utf8(chunk)
                .ok()
                .and_then(|s| s.chars().next())
            else {
                out.extend_from_slice(chunk);
                rest = &rest[len..];
                continue;
            };

            // Where the character will land, asked before it is fed: afterwards the
            // cursor has already moved past it.
            let (row, col) = terminal.screen().cursor_position();
            terminal.process(chunk);

            let (fg, bg) = match terminal.screen().cell(row, col) {
                Some(cell) => (cell.fgcolor(), cell.bgcolor()),
                None => (Color::Default, Color::Default),
            };
            let drawn = Cell {
                symbol: ch,
                fg,
                bg,
            };
            let wanted = Map::map(map, drawn);

            if wanted == drawn {
                out.extend_from_slice(chunk);
            } else {
                // The replacement's own colours, then its symbol, then the colours
                // the stream has in effect so the next cell is not affected by it.
                push_sgr(&mut out, wanted.fg, wanted.bg);
                let mut buf = [0u8; 4];
                out.extend_from_slice(wanted.symbol.encode_utf8(&mut buf).as_bytes());
                push_sgr(&mut out, fg, bg);
            }
            rest = &rest[len..];
        }

        out
    }
}

/// Append the SGR that selects `fg` and `bg`.
fn push_sgr(out: &mut Vec<u8>, fg: Color, bg: Color) {
    use std::io::Write as _;
    let _ = write!(out, "\x1b[0");
    push_colour(out, 30, 38, fg);
    push_colour(out, 40, 48, bg);
    let _ = write!(out, "m");
}

fn push_colour(out: &mut Vec<u8>, basic: u8, extended: u8, colour: Color) {
    use std::io::Write as _;
    match colour {
        Color::Default => {}
        Color::Idx(index) if index < 8 => {
            let _ = write!(out, ";{}", basic + index);
        }
        Color::Idx(index) if index < 16 => {
            // The bright eight sit above the basic ones.
            let _ = write!(out, ";{}", basic + 60 + (index - 8));
        }
        Color::Idx(index) => {
            let _ = write!(out, ";{};5;{}", extended, index);
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";{};2;{};{};{}", extended, r, g, b);
        }
    }
}

/// The escape sequence starting at `bytes[0]`, and everything after it.
fn split_escape(bytes: &[u8]) -> (&[u8], &[u8]) {
    let mut i = 1;
    match bytes.get(i) {
        // `CSI` and `SS3`: parameters and intermediates, then a final byte in
        // `@`..=`~`.
        Some(b'[') | Some(b'O') => {
            i += 1;
            while let Some(&byte) = bytes.get(i) {
                i += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        }
        // A string sequence: `OSC`, `DCS`, `APC`, `PM`, `SOS`. Runs to `BEL` or to
        // `ESC \`.
        Some(b']') | Some(b'_') | Some(b'P') | Some(b'^') | Some(b'X') => {
            i += 1;
            while let Some(&byte) = bytes.get(i) {
                if byte == 0x07 {
                    i += 1;
                    break;
                }
                if byte == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    break;
                }
                i += 1;
            }
        }
        // Two bytes: `ESC` and its final.
        Some(_) => i += 1,
        None => {}
    }
    bytes.split_at(i.min(bytes.len()))
}

/// How many bytes the UTF-8 character starting with `lead` occupies.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cell as it was drawn: the callback's own view of the stream.
    fn unchanged(cell: Cell) -> Cell {
        cell
    }

    /// A terminal to rewrite into, sized so a test's text fits on one row.
    fn terminal() -> vt100::Parser {
        vt100::Parser::new(
            std::num::NonZeroU16::new(3).expect("rows"),
            std::num::NonZeroU16::new(20).expect("cols"),
            0,
        )
    }

    #[test]
    fn an_untouched_stream_is_byte_identical() {
        let mut map = CellMap::new();
        let wrote = b"hello \x1b[31mred\x1b[0m and \x1b]0;a title\x07plain";
        assert_eq!(
            map.map(&mut terminal(), wrote, &mut unchanged),
            wrote.to_vec()
        );
    }

    #[test]
    fn a_rewritten_cell_reports_the_colours_in_effect() {
        let mut map = CellMap::new();
        let seen = std::cell::RefCell::new(Vec::new());
        map.map(&mut terminal(), b"\x1b[31;42mx", &mut |cell| {
            seen.borrow_mut().push(cell);
            cell
        });
        assert_eq!(
            *seen.borrow(),
            vec![Cell {
                symbol: 'x',
                fg: Color::Idx(1),
                bg: Color::Idx(2),
            }]
        );
    }

    #[test]
    fn a_replacement_carries_its_own_colours_and_restores_the_streams() {
        let mut map = CellMap::new();
        let wrote = map.map(&mut terminal(), b"\x1b[31mx", &mut |_| Cell {
            symbol: ' ',
            fg: Color::Default,
            bg: Color::Default,
        });
        let text = String::from_utf8(wrote).expect("escapes are text");
        // The replacement's own SGR, its symbol, then the stream's SGR back.
        assert!(text.contains("\x1b[0m "), "{text:?}");
        assert!(text.ends_with("\x1b[0;31m"), "{text:?}");
    }

    #[test]
    fn an_escape_is_never_split_by_a_rewrite() {
        let mut map = CellMap::new();
        let wrote = map.map(&mut terminal(), b"\x1b]0;title\x07a", &mut |_| Cell {
            symbol: 'b',
            fg: Color::Default,
            bg: Color::Default,
        });
        let text = String::from_utf8(wrote).expect("escapes are text");
        assert!(text.starts_with("\x1b]0;title\x07"), "{text:?}");
    }

    #[test]
    fn a_character_split_across_reads_waits_for_its_bytes() {
        let mut map = CellMap::new();
        let mut terminal = terminal();
        // The first byte of a two-byte character, alone.
        let first = map.map(&mut terminal, &[0xc3], &mut unchanged);
        assert!(first.is_empty(), "half a character is not written: {first:?}");
        // The second completes it, and it is written whole.
        let second = map.map(&mut terminal, &[0xa9], &mut unchanged);
        assert_eq!(second, vec![0xc3, 0xa9]);
    }
}

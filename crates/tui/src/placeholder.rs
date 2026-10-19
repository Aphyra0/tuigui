//! Turning a pane's fill into the cells that anchor an image.
//!
//! The client draws a solid fill of one character to say "this pane is mine and
//! this is its grid". Those cells are the *anchor*: this side swaps each one for
//! kitty's image placeholder, so the terminal resolves the cell against the image
//! the server has placed, and the client needs to know nothing about graphics at
//! all. A fill is a thing any program can draw; placeholders are this side's job
//! because only this side is talking to a terminal.
//!
//! ```text
//!   ❋❋❋❋  ──▶  ESC[38;2;0;0;99m U+10EEEE U+0305 U+0305  ×4
//!               (id 99 in the fg)   (row 1)  (col 1)
//! ```
//!
//! # Why a colour carries the id
//!
//! The protocol encodes the image id in the cell's foreground colour, and its row
//! and column as combining diacritics after the placeholder. So the mapping is
//! per cell and needs the cell's position, which is why the rewrite tracks the
//! cursor: the fill itself is written as plain text and says nothing about where
//! it is.

/// The character a client fills its pane with.
///
/// One character, and not a sequence, because a fill is drawn a screen at a time
/// and anything longer would make the rewrite's per-cell cost the size of the
/// marker.
pub const FILL: char = '❋';

/// The image id the placeholder cells name, in their foreground colour.
///
/// Re-exported from the session, because the two numbers are one fact: this module
/// writes the id into the cells and the session creates the image under it, so a
/// second copy here would be a copy that could drift and make every cell reference
/// an image that does not exist.
pub use crate::session::IMAGE_ID;

/// Rewrites a pane's bytes, replacing [`FILL`] with image placeholders.
///
/// Stateful because a placeholder's row and column are where the cell is, and
/// where the cell is depends on everything before it: cursor moves and the cells
/// already emitted. Also stateful across calls, because a read can end in the
/// middle of a multi-byte character and the remainder has to wait for the bytes
/// that complete it.
pub struct Placeholders {
    /// The cursor, in cells, zero-based, as the escapes have described it.
    row: u32,
    col: u32,
    /// Bytes of a character that was cut off by the end of a read.
    carry: Vec<u8>,
    /// Whether the last thing emitted was a placeholder, so the foreground colour
    /// that carries the id is set only when it is needed and cleared as soon as it
    /// is not.
    in_placeholder: bool,
}

impl Placeholders {
    pub fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            carry: Vec::new(),
            in_placeholder: false,
        }
    }

    /// Rewrite `bytes`, ready to be written to the terminal.
    ///
    /// Every byte is either copied through or replaced by one placeholder, so the
    /// output is a superset of the input in size and identical in meaning. An
    /// incomplete character at the end is held back rather than emitted, so a fill
    /// split across two reads still becomes one placeholder.
    pub fn rewrite(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());

        // Anything held from the last call is the beginning of this one.
        let mut input = Vec::with_capacity(self.carry.len() + bytes.len());
        input.append(&mut self.carry);
        input.extend_from_slice(bytes);

        let mut i = 0;
        while i < input.len() {
            let byte = input[i];

            // An escape sequence is copied whole and, if it is a cursor move, is
            // where the position comes from. Nothing else in the stream moves the
            // cursor: a draw emits a move per row and prints the row without ever
            // issuing a newline.
            if byte == 0x1b {
                match escape_end(&input, i) {
                    Some(end) => {
                        self.note_cursor(&input[i..end]);
                        self.leave_placeholder(&mut out);
                        out.extend_from_slice(&input[i..end]);
                        i = end;
                        continue;
                    }
                    // The escape ran off the end of the read. Hold it for the rest.
                    None => {
                        self.carry.extend_from_slice(&input[i..]);
                        return out;
                    }
                }
            }

            // Text is taken only up to the next escape, never to the end of the
            // buffer. An `ESC` is valid UTF-8, so a decode of everything that follows
            // succeeds and every later escape is handled as ordinary characters --
            // which means only the first cursor move in a read is ever seen, and every
            // row after it is drawn with the row it started on.
            let end = match input[i..].iter().position(|&b| b == 0x1b) {
                Some(at) => i + at,
                None => input.len(),
            };

            match std::str::from_utf8(&input[i..end]) {
                Ok(text) => {
                    for ch in text.chars() {
                        self.cell(ch, &mut out);
                    }
                    i = end;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // The prefix is whole text, so it can be handled now.
                        let text = std::str::from_utf8(&input[i..i + valid]).expect("validated");
                        for ch in text.chars() {
                            self.cell(ch, &mut out);
                        }
                        i += valid;
                    }
                    if end == input.len() {
                        // A character cut off by the end of the read: it waits for the
                        // bytes that complete it.
                        self.carry.extend_from_slice(&input[i..]);
                        return out;
                    }
                    // Malformed bytes with an escape after them: they cannot be text,
                    // and skipping them is what makes progress.
                    i = end;
                }
            }
        }
        out
    }

    /// One character at the cursor, which then advances.
    ///
    /// A fill is a placeholder; anything else is itself. Either way the cursor
    /// moves one column, because a fill and a placeholder both occupy exactly one
    /// cell and the row the client is drawing is the row that ends up on screen.
    fn cell(&mut self, ch: char, out: &mut Vec<u8>) {
        if ch == FILL {
            if !self.in_placeholder {
                // The id, as `color_to_id` reads it: the 24-bit colour *is* the id,
                // so 99 is red 0, green 0, blue 99.
                let (r, g, b) = id_rgb(IMAGE_ID);
                out.extend_from_slice(format!("\x1b[38;2;{r};{g};{b}m").as_bytes());
                self.in_placeholder = true;
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(PLACEHOLDER.encode_utf8(&mut buf).as_bytes());
            // Row and column are one-based, and zero means "same as the cell to the
            // left", so a cell at the very start of a row must say `1` explicitly.
            if let Some(d) = diacritic(self.row + 1) {
                out.extend_from_slice(d.encode_utf8(&mut buf).as_bytes());
            }
            if let Some(d) = diacritic(self.col + 1) {
                out.extend_from_slice(d.encode_utf8(&mut buf).as_bytes());
            }
        } else {
            self.leave_placeholder(out);
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        self.col += 1;
    }

    /// Give the foreground colour back before anything that is not a placeholder.
    ///
    /// The id lives in that colour, so leaving it set would make every later cell
    /// on the screen the colour of an image id instead of whatever the pane meant.
    fn leave_placeholder(&mut self, out: &mut Vec<u8>) {
        if self.in_placeholder {
            out.extend_from_slice(b"\x1b[39m");
            self.in_placeholder = false;
        }
    }

    /// Take the cursor position out of an escape, if it is one that sets it.
    fn note_cursor(&mut self, escape: &[u8]) {
        // `ESC [ <row> ; <col> H`, which is what a cursor move is. Anything else —
        // attributes, a mode set, a clear — leaves the cursor where it was.
        let Some(body) = escape
            .strip_prefix(b"\x1b[")
            .and_then(|rest| rest.strip_suffix(b"H"))
        else {
            return;
        };
        let body = std::str::from_utf8(body).unwrap_or("");
        let mut parts = body.split(';');
        if let (Some(row), Some(col)) = (parts.next(), parts.next()) {
            if let (Ok(row), Ok(col)) = (row.parse::<u32>(), col.parse::<u32>()) {
                self.row = row.saturating_sub(1);
                self.col = col.saturating_sub(1);
            }
        }
    }
}

/// The end of the escape sequence starting at `start`, exclusive.
///
/// `None` while the sequence is unfinished, which is not a failure: an escape can
/// straddle two reads, and the caller holds it until it does not.
fn escape_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    match bytes.get(i) {
        // `ESC [` and `ESC O`: a control sequence, terminated by a byte in the
        // final position's range.
        Some(b'[') | Some(b'O') => {
            i += 1;
            while i < bytes.len() {
                // The final byte of a CSI is `@` through `~`; every parameter and
                // intermediate byte is below that range.
                if (0x40..=0x7e).contains(&bytes[i]) {
                    return Some(i + 1);
                }
                i += 1;
            }
            None
        }
        // A two-byte escape, or a longer string one whose body is copied through.
        Some(b']') | Some(b'_') | Some(b'P') | Some(b'^') => {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return Some(i + 1);
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return Some(i + 2);
                }
                i += 1;
            }
            None
        }
        Some(_) => Some(start + 2),
        None => None,
    }
}

/// The id as a 24-bit colour: the id itself, since that is what the terminal reads
/// back out of it (`color_to_id` masks the 24 most significant bits).
fn id_rgb(id: u32) -> (u8, u8, u8) {
    (
        ((id >> 16) & 0xff) as u8,
        ((id >> 8) & 0xff) as u8,
        (id & 0xff) as u8,
    )
}

/// The placeholder character, `U+10EEEE`.
const PLACEHOLDER: char = '\u{10eeee}';

/// The diacritic that encodes `n`, for row and column indices.
///
/// Kitty's list of combining marks, as `(low, high, first)`: the marks
/// `low..=high` encode the indices `first..` in order. Transcribed from the
/// protocol's own `rowcolumn-diacritics.txt`, where the index is the entry's
/// position — one-based, which is why an index of 1 is the first mark.
///
/// The list and not the terminal's *decode* table: that one is range-compressed
/// and its ranges overlap (index 2 is reached by two different marks), which is
/// harmless for decoding and wrong for encoding. Every index here has exactly one
/// mark, so a cell names one position and only one.
const DIACRITICS: &[(u32, u32, u32)] = &[
    (0x0305, 0x0305, 1),
    (0x030d, 0x030e, 2),
    (0x0310, 0x0310, 4),
    (0x0312, 0x0312, 5),
    (0x033d, 0x033f, 6),
    (0x0346, 0x0346, 9),
    (0x034a, 0x034c, 10),
    (0x0350, 0x0352, 13),
    (0x0357, 0x0357, 16),
    (0x035b, 0x035b, 17),
    (0x0363, 0x036f, 18),
    (0x0483, 0x0487, 31),
    (0x0592, 0x0595, 36),
    (0x0597, 0x0599, 40),
    (0x059c, 0x05a1, 43),
    (0x05a8, 0x05a9, 49),
    (0x05ab, 0x05ac, 51),
    (0x05af, 0x05af, 53),
    (0x05c4, 0x05c4, 54),
    (0x0610, 0x0617, 55),
    (0x0657, 0x065b, 63),
    (0x065d, 0x065e, 68),
    (0x06d6, 0x06dc, 70),
    (0x06df, 0x06e2, 77),
    (0x06e4, 0x06e4, 81),
    (0x06e7, 0x06e8, 82),
    (0x06eb, 0x06ec, 84),
    (0x0730, 0x0730, 86),
    (0x0732, 0x0733, 87),
    (0x0735, 0x0736, 89),
    (0x073a, 0x073a, 91),
    (0x073d, 0x073d, 92),
    (0x073f, 0x0741, 93),
    (0x0743, 0x0743, 96),
    (0x0745, 0x0745, 97),
    (0x0747, 0x0747, 98),
    (0x0749, 0x074a, 99),
    (0x07eb, 0x07f1, 101),
    (0x07f3, 0x07f3, 108),
    (0x0816, 0x0819, 109),
    (0x081b, 0x0823, 113),
    (0x0825, 0x0827, 122),
    (0x0829, 0x082d, 125),
    (0x0951, 0x0951, 130),
    (0x0953, 0x0954, 131),
    (0x0f82, 0x0f83, 133),
    (0x0f86, 0x0f87, 135),
    (0x135d, 0x135f, 137),
    (0x17dd, 0x17dd, 140),
    (0x193a, 0x193a, 141),
    (0x1a17, 0x1a17, 142),
    (0x1a75, 0x1a7c, 143),
    (0x1b6b, 0x1b6b, 151),
    (0x1b6d, 0x1b73, 152),
    (0x1cd0, 0x1cd2, 159),
    (0x1cda, 0x1cdb, 162),
    (0x1ce0, 0x1ce0, 164),
    (0x1dc0, 0x1dc1, 165),
    (0x1dc3, 0x1dc9, 167),
    (0x1dcb, 0x1dcc, 174),
    (0x1dd1, 0x1de6, 176),
    (0x1dfe, 0x1dfe, 198),
    (0x20d0, 0x20d1, 199),
    (0x20d4, 0x20d7, 201),
    (0x20db, 0x20dc, 205),
    (0x20e1, 0x20e1, 207),
    (0x20e7, 0x20e7, 208),
    (0x20e9, 0x20e9, 209),
    (0x20f0, 0x20f0, 210),
    (0x2cef, 0x2cf1, 211),
    (0x2de0, 0x2dff, 214),
    (0xa66f, 0xa66f, 246),
    (0xa67c, 0xa67d, 247),
    (0xa6f0, 0xa6f1, 249),
    (0xa8e0, 0xa8f1, 251),
    (0xaab0, 0xaab0, 269),
    (0xaab2, 0xaab3, 270),
    (0xaab7, 0xaab8, 272),
    (0xaabe, 0xaabf, 274),
    (0xaac1, 0xaac1, 276),
    (0xfe20, 0xfe26, 277),
    (0x10a0f, 0x10a0f, 284),
    (0x10a38, 0x10a38, 285),
    (0x1d185, 0x1d189, 286),
    (0x1d1aa, 0x1d1ad, 291),
    (0x1d242, 0x1d244, 295),
];

/// The diacritic that encodes `n`, if the table has one.
fn diacritic(n: u32) -> Option<char> {
    for &(low, high, first) in DIACRITICS {
        let last = first + (high - low);
        if (first..=last).contains(&n) {
            return char::from_u32(low + (n - first));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rewrite `input` in one call and report it as text.
    fn once(input: &str) -> String {
        String::from_utf8(Placeholders::new().rewrite(input.as_bytes())).expect("rewrite is text")
    }

    /// The id the cells carry is the one the terminal reads back out of the colour.
    ///
    /// Kitty's `color_to_id` keeps the 24 most significant bits, so the colour *is*
    /// the id and the two numbers in this codebase have to agree. This asserts the
    /// whole path: what is emitted, and what a terminal would decode from it.
    #[test]
    fn the_colour_carries_the_image_id() {
        let out = once("❋");
        assert!(out.starts_with("\x1b[38;2;0;0;99m"), "{out:?}");
        // What the terminal decodes: it packs the 24-bit colour at the top of a
        // 32-bit value and shifts it back down, so the colour is the id itself.
        let colour = (0u32 << 16) | (0 << 8) | 99;
        assert_eq!(colour, IMAGE_ID);
    }

    /// A fill becomes the placeholder character, one cell for one cell.
    #[test]
    fn a_fill_becomes_a_placeholder() {
        let out = once("❋");
        // The colour, then the character, then the row and column diacritics.
        let cells: Vec<char> = out
            .trim_start_matches("\x1b[38;2;0;0;99m")
            .chars()
            .collect();
        assert_eq!(cells[0], '\u{10eeee}');
        // Row 1 is `U+0305` and column 1 is `U+0305`: the protocol's index 1.
        assert_eq!(cells[1], '\u{0305}');
        assert_eq!(cells[2], '\u{0305}');
    }

    /// Text that is not a fill is passed through unchanged.
    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(once("unix : /tmp/x.sock"), "unix : /tmp/x.sock");
    }

    /// A row of cells counts across, so the column in each cell is its own.
    ///
    /// The diacritics are what tell the terminal which part of the image a cell
    /// shows: getting the column wrong draws the same slice of the picture in every
    /// cell, which is a wrong picture rather than no picture. A cell carries the row
    /// first and the column second, and both are one-based, so a cell on the first
    /// row names row 1 and its own column.
    #[test]
    fn each_cell_carries_its_own_column() {
        let out = once("❋❋❋");
        let body = out.trim_start_matches("\x1b[38;2;0;0;99m");
        let cells: Vec<Vec<char>> = body
            .split('\u{10eeee}')
            .skip(1)
            .map(|cell| cell.chars().take(2).collect())
            .collect();
        assert_eq!(cells.len(), 3);
        // Row 1 (`U+0305`) for all three, and then columns 1, 2 and 3.
        assert_eq!(cells[0], vec!['\u{0305}', '\u{0305}']);
        assert_eq!(cells[1], vec!['\u{0305}', '\u{030d}']);
        assert_eq!(cells[2], vec!['\u{0305}', '\u{030e}']);
    }

    /// A cursor move sets the row, so the next fill names the row it is on.
    #[test]
    fn a_cursor_move_sets_the_row() {
        // To row 2, column 1, then one fill: row 2 is the protocol's `U+030D`.
        let out = once("\x1b[2;1H❋");
        let after = out.split('\u{10eeee}').nth(1).expect("a placeholder");
        assert!(after.starts_with('\u{030d}'), "{after:?}");
    }

    /// Every cursor move in a read is seen, not only the first.
    ///
    /// This is what a drawn screen actually looks like: a move per row, and a fill for
    /// that row. An `ESC` is valid UTF-8, so decoding everything after the first move
    /// succeeds and treats the remaining moves as ordinary characters — which leaves the
    /// row stuck at whatever the first move set, and draws every row as the first one.
    /// The picture on screen is then a correct horizontal line, repeated down the pane.
    #[test]
    fn each_rows_move_sets_that_rows_diacritic() {
        let out = once("\x1b[1;1H❋\x1b[2;1H❋\x1b[3;1H❋");
        let rows: Vec<char> = out
            .split('\u{10eeee}')
            .skip(1)
            .map(|cell| cell.chars().next().expect("a row diacritic"))
            .collect();
        // Rows 1, 2 and 3: `U+0305`, `U+030D`, `U+030E`.
        assert_eq!(rows, vec!['\u{0305}', '\u{030d}', '\u{030e}'], "{out:?}");
    }

    /// A character split across two reads still becomes one placeholder.
    ///
    /// Reads end wherever the kernel decides, and the fill is a three-byte character,
    /// so this is the normal case rather than an edge one.
    #[test]
    fn a_character_split_across_reads_is_reassembled() {
        let mut state = Placeholders::new();
        let bytes = "❋".as_bytes();
        let (head, tail) = bytes.split_at(1);

        let first = state.rewrite(head);
        let second = state.rewrite(tail);

        // Nothing can be emitted from a third of a character, so the first read
        // yields only the colour if anything at all, and the whole placeholder comes
        // out of the second.
        let mut joined = first;
        joined.extend_from_slice(&second);
        let text = String::from_utf8(joined).expect("reassembled text");
        assert!(text.contains('\u{10eeee}'), "{text:?}");
    }

    /// The colour is given back as soon as a cell is not a placeholder.
    ///
    /// The id lives in that colour, so leaving it set would draw every later cell on
    /// the screen in the colour of an image id.
    #[test]
    fn the_colour_is_cleared_after_the_fill() {
        let out = once("❋x");
        assert!(out.contains('\u{10eeee}'), "{out:?}");
        assert!(out.contains("\x1b[39m"), "the colour was not reset: {out:?}");
        assert!(out.ends_with('x'), "{out:?}");
    }

    /// An escape that is not a cursor move does not move the cursor.
    ///
    /// SGR and mode sets are everywhere in a drawn frame; if any of them reset the
    /// row, every cell after the first attribute change would name row 1.
    #[test]
    fn an_attribute_escape_does_not_move_the_cursor() {
        let mut state = Placeholders::new();
        let _ = state.rewrite("\x1b[2;1H".as_bytes());
        let _ = state.rewrite("\x1b[39m".as_bytes());
        let out = String::from_utf8(state.rewrite("❋".as_bytes())).expect("text");
        let after = out.split('\u{10eeee}').nth(1).expect("a placeholder");
        assert!(after.starts_with('\u{030d}'), "row 2 expected: {after:?}");
    }

    /// Row and column survive a read boundary between them.
    ///
    /// The cursor is state, so a fill that begins in one read has to be named with
    /// the position the previous reads established, not with the start of the buffer.
    #[test]
    fn the_cursor_survives_across_reads() {
        let mut state = Placeholders::new();
        let _ = state.rewrite("\x1b[3;5H".as_bytes());
        let out = String::from_utf8(state.rewrite("❋".as_bytes())).expect("text");
        let after = out.split('\u{10eeee}').nth(1).expect("a placeholder");
        let diacritics: Vec<char> = after.chars().take(2).collect();
        // Row 3 is index 3 (`U+030e`) and column 5 is index 5 (`U+0312`).
        assert_eq!(diacritics, vec!['\u{030e}', '\u{0312}'], "{after:?}");
    }

    /// Every entry in the table is reachable, and the table has no gaps.
    ///
    /// The marks are a dense sequence: index `n` maps to a mark, and the next index
    /// to the next mark. A transcription error in one entry would shift every index
    /// after it, so this walks the whole flat range and checks it is contiguous.
    #[test]
    fn the_table_covers_every_index_without_a_gap() {
        let mut expected = 1;
        for &(low, high, first) in DIACRITICS {
            assert_eq!(first, expected, "the table has a gap or an overlap at {low:#x}");
            assert!(high >= low);
            expected += high - low + 1;
        }
        // One past the last index in the table, which is what a long row needs.
        assert_eq!(expected, 298);
        // And the marks are where the table says they are.
        assert_eq!(diacritic(1), char::from_u32(0x0305));
        assert_eq!(diacritic(2), char::from_u32(0x030d));
        assert_eq!(diacritic(297), char::from_u32(0x1d244));
        assert_eq!(diacritic(298), None);
    }
}

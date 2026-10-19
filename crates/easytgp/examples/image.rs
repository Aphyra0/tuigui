//! A scratch example, kept for the snippets in its comments.
//!
//! It does nothing: every line is commented out. Left in place rather than deleted because
//! the commented calls are the record of how to reach the pieces an example would need —
//! `Image::display()` and the terminal-size probe — and re-deriving them is the work this
//! file exists to save.

fn main() {
    // dbg!(rustix::termios::tcgetwinsize(std::io::stdin()).unwrap());

    // Image::display();
}

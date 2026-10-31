# tuigui

[![Rust lines of code](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2F4phyra%2Ftuigui%2Fmaster%2F.github%2Ftokei.json)](https://github.com/4phyra/tuigui)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)

tuigui puts graphical programs in the terminal, alongside the text tools you
already use. You give it a program; it gives you that program's window and
video in the pane, through the kitty graphics protocol, while keeping the
terminal's normal input and output intact. The same split between the terminal
side and the program side is intended to let a program run on another machine
and still be used from your terminal.

## Thanks

Built on excellent crates:

- [smithay](https://github.com/Smithay/smithay), the Wayland compositor library
- [vt100](https://github.com/doy/vt100) and
  [termina](https://github.com/helix-editor/termina), terminal emulation and input
- [rkyv](https://github.com/rkyv/rkyv), message format
- [tokio](https://github.com/tokio-rs/tokio), async runtime
- [rustix](https://github.com/bytecodealliance/rustix), system calls

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

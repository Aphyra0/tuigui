# Build and run the three binaries.
#
# The recipes exist because the invocations are long and fixed: a target directory that must
# be `./target` (a different one invalidates every fingerprint and rebuilds the TLS stack), a
# package to build, and a binary to run. Spelling that out each time is where it goes wrong.
#
# Run inside the dev shell (`nix develop`), which is where cargo and the system libraries it
# links against come from.

target := "./target"

# Build the server around a command, then run it in this terminal.
#
# The command is what the server puts on a pty, so `just tui ./target/release/gui` runs the
# client inside the server — which is the usual shape, since the client announces itself in
# the pane's output and the server is what reads it.
tui *command:
    CARGO_TARGET_DIR={{target}} cargo build --release -p tui --bins
    {{target}}/release/tui {{command}}

# Build and run the client.
#
# On its own it binds a socket and waits to be found, so it does nothing useful until a
# server is reading the pane it draws in. `just tui` with this as the command is the usual
# way to start it.
gui *args:
    CARGO_TARGET_DIR={{target}} cargo build --release -p gui --bins
    {{target}}/release/gui {{args}}

# Build and run both halves as one command: the server owns this terminal, and the client
# runs inside the pty it creates.
tuigui *args:
    CARGO_TARGET_DIR={{target}} cargo build --release -p tuigui --bins
    {{target}}/release/tuigui {{args}}

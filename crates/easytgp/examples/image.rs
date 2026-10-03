use std::io;

use easytgp::Image;

fn main() {
    // dbg!(rustix::termios::tcgetwinsize(io::stdin()).unwrap());

    Image::display();
}

//! End-to-end test: spawn a real wayland client against our compositor and
//! drive it through capture + input.
//!
//! Marked `#[ignore]` because it spawns processes and needs a working
//! Wayland environment; run it explicitly:
//!
//! ```sh
//! cargo test -p gobo-comp --test e2e -- --nocapture
//! ```

use gobo_comp::{Button, Compositor, Key, ScrollDelta};

#[test]
fn spawn_capture_and_input() {
    let comp = Compositor::init((320, 240), 1.0).expect("init compositor");
    let client = comp
        .spawn(env!("CARGO_BIN_EXE_gobo-comp-smoke"), (320, 240))
        .expect("spawn smoke client");

    // Pixel sampler: read the RGB triple at (x, y).
    let px = |f: &gobo_comp::Frame, x: usize, y: usize| {
        let o = (y * f.width as usize + x) * 4;
        (f.rgba[o], f.rgba[o + 1], f.rgba[o + 2])
    };

    // 1. First capture: the client's initial solid color.
    let f1 = client.capture().expect("first capture");
    assert_eq!((f1.width, f1.height), (320, 240));
    assert_eq!(f1.rgba.len(), 320 * 240 * 4);
    assert_eq!(px(&f1, 10, 10), (0x10, 0x20, 0x40));

    // 2. Key input: the smoke client repaints on each key press.
    client.type_key(Key::Char('a')).expect("type a");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let f2 = client.capture().expect("capture after key");
    assert_eq!(px(&f2, 10, 10), (0x40, 0x20, 0x10));

    // 3. Pointer click: another repaint color.
    client.move_mouse(160.0, 120.0).expect("move");
    client.click(Button::Left).expect("click");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let f3 = client.capture().expect("capture after click");
    assert_eq!(px(&f3, 10, 10), (0x10, 0x60, 0x10));

    // 4. Scroll: yet another repaint color.
    client.scroll(ScrollDelta::vertical(1)).expect("scroll");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let f4 = client.capture().expect("capture after scroll");
    assert_eq!(px(&f4, 10, 10), (0x60, 0x60, 0x10));

    assert!(client.is_running());
    drop(client);
    comp.shutdown();
}

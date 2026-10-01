fn main() {
    let comp = tuigui_comp::Compositor::init((320, 240), 1.0).unwrap();
    println!("socket: {:?}", comp.wayland_socket());
    let smoke = std::env::args().nth(1).expect("usage: comp-debug <smoke-bin>");
    let client = comp.spawn(&smoke, (320, 240)).expect("spawn");
    println!("spawned ok!");

    let px = |f: &tuigui_comp::Frame, x: usize, y: usize| {
        let o = (y * f.width as usize + x) * 4;
        (f.rgba[o], f.rgba[o + 1], f.rgba[o + 2])
    };
    let f1 = client.capture().unwrap();
    println!("f1 px={:?}", px(&f1, 10, 10));

    client.type_key(tuigui_comp::Key::Char('a')).expect("type a");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let f2 = client.capture().unwrap();
    println!("f2 px={:?} (after key)", px(&f2, 10, 10));

    client.move_mouse(160.0, 120.0).expect("move");
    client.click(tuigui_comp::Button::Left).expect("click");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let f3 = client.capture().unwrap();
    println!("f3 px={:?} (after click)", px(&f3, 10, 10));

    client.scroll(tuigui_comp::ScrollDelta::vertical(1)).expect("scroll");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let f4 = client.capture().unwrap();
    println!("f4 px={:?} (after scroll)", px(&f4, 10, 10));
}

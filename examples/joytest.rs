use std::time::{Duration, Instant};
fn main() {
    let mut js = wedo::joystick::Joystick::open().expect("joystick");
    println!("opened: {}\naxes: {:?}\nbuttons: {:?}", js.name, js.axis_names(), js.button_names());
    let names = js.button_names();
    let t = Instant::now();
    let mut n = 0;
    while t.elapsed() < Duration::from_secs(5) {
        if let Some(s) = js.read(Duration::from_millis(100)).unwrap() {
            n += 1;
            if n % 40 == 1 {
                let pressed: Vec<_> = names.iter().zip(&s.buttons).filter(|(_, &b)| b).map(|(n, _)| n).collect();
                println!("axes {:?} pressed {pressed:?}", s.axes.iter().map(|v| (v * 100.0).round() as i32).collect::<Vec<_>>());
            }
        }
    }
    println!("{n} reports in 5 s");
}

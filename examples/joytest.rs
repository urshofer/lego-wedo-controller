use std::time::{Duration, Instant};
fn main() {
    let mut js = wedo::joystick::Joystick::open().expect("joystick");
    println!("opened: {}", js.name);
    let t = Instant::now();
    let mut n = 0;
    while t.elapsed() < Duration::from_secs(3) {
        if let Some(s) = js.read(Duration::from_millis(100)).unwrap() {
            n += 1;
            if n % 25 == 1 { println!("{s:?}"); }
        }
    }
    println!("{n} reports in 3 s");
}

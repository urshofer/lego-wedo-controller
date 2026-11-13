//! Logs the raw port IDs while the motor on port A runs at several speeds.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use wedo::{Kind, Port, WeDo};

fn main() {
    let mut hub = WeDo::open().expect("hub");
    for speed in [0, 20, 50, 100, -50, -100, 0] {
        hub.set_motor(Port::A, speed).unwrap();
        let mut ids: BTreeMap<u8, u32> = BTreeMap::new();
        let mut kinds: Vec<String> = vec![];
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(800) {
            if let Some(r) = hub.read(Duration::from_millis(100)).unwrap() {
                *ids.entry(r.raw[0].0).or_default() += 1;
                let k = format!("{:?}", r.a.kind());
                if kinds.last() != Some(&k) { kinds.push(k); }
            }
        }
        let named: Vec<String> = ids.iter().map(|(id, n)| format!("{id}({:?})x{n}", Kind::from_id(*id))).collect();
        println!("speed {speed:4}: ids {}  | debounced: {}", named.join(" "), kinds.join(" -> "));
    }
    hub.stop().unwrap();
}

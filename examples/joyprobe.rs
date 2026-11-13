use std::time::{Duration, Instant};
fn main() {
    let api = hidapi::HidApi::new().unwrap();
    let info = api.device_list().find(|d| d.vendor_id() == 0x1b4f && d.usage() == 0x04).expect("joystick");
    let dev = info.open_device(&api).unwrap();
    let mut desc = [0u8; 4096];
    let n = dev.get_report_descriptor(&mut desc).unwrap();
    println!("descriptor ({n} bytes): {}", desc[..n].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
    let t = Instant::now();
    let mut last = vec![];
    while t.elapsed() < Duration::from_secs(4) {
        let mut buf = [0u8; 64];
        let n = dev.read_timeout(&mut buf, 100).unwrap();
        if n > 0 && buf[..n] != last[..] { println!("{:?}", &buf[..n]); last = buf[..n].to_vec(); }
    }
}

use std::time::{Duration, Instant};
fn main() {
    let h = rusb::open_device_with_vid_pid(0x046d, 0xc21d).expect("F310 (X mode)");
    let _ = h.set_auto_detach_kernel_driver(true);
    let dev = h.device();
    let cfg = dev.config_descriptor(0).unwrap(); println!("set cfg: {:?}", h.set_active_configuration(cfg.number()));
    for i in cfg.interfaces() { for d in i.descriptors() {
        println!("if {} class {:02x}/{:02x}/{:02x}", d.interface_number(), d.class_code(), d.sub_class_code(), d.protocol_code());
        for e in d.endpoint_descriptors() { println!("  ep {:02x} {:?} {:?} max {}", e.address(), e.direction(), e.transfer_type(), e.max_packet_size()); }
    }}
    h.claim_interface(0).expect("claim");
    let t = Instant::now(); let mut last = vec![];
    while t.elapsed() < Duration::from_secs(6) {
        let mut buf = [0u8; 32];
        match h.read_interrupt(0x81, &mut buf, Duration::from_millis(100)) {
            Ok(n) if buf[..n] != last[..] => { println!("{:02x?}", &buf[..n]); last = buf[..n].to_vec(); }
            Ok(_) | Err(rusb::Error::Timeout) => {}
            Err(e) => { println!("err {e}"); break; }
        }
    }
}

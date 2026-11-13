//! Generic USB HID joystick: finds X, Y and button 1 by parsing the HID
//! report descriptor, and calibrates itself from what it sees.
//!
//! Calibration: the centre is the average of the first reports after
//! connecting (leave the stick at rest), and each direction's range grows to
//! the furthest point the stick has reached. Until a direction has been pushed
//! at least halfway out, half the logical range is used as its extent, so a
//! small first move never jumps to full power.

use hidapi::{HidApi, HidDevice, HidError};
use std::time::Duration;

const CENTER_SAMPLES: u32 = 16;
const DEADZONE: f32 = 0.08;
/// A direction counts as calibrated once it reached this share of the half range.
const MIN_EXTENT: f32 = 0.5;

#[derive(Debug, Clone, Copy)]
struct Field {
    report_id: u8,
    bit_offset: usize,
    bit_size: usize,
    logical_min: i32,
    logical_max: i32,
}

#[derive(Debug, Clone, Copy)]
struct Layout {
    x: Field,
    y: Field,
    button: Option<Field>,
    uses_report_ids: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AxisCal {
    sum: f32,
    samples: u32,
    center: f32,
    lo: f32,
    hi: f32,
    half: f32,
}

impl AxisCal {
    fn new(f: &Field) -> Self {
        Self { half: (f.logical_max - f.logical_min) as f32 / 2.0, ..Default::default() }
    }

    fn centered(&self) -> bool {
        self.samples >= CENTER_SAMPLES
    }

    fn feed(&mut self, v: f32) {
        if !self.centered() {
            self.sum += v;
            self.samples += 1;
            if self.centered() {
                self.center = self.sum / self.samples as f32;
                self.lo = self.center;
                self.hi = self.center;
            }
            return;
        }
        self.lo = self.lo.min(v);
        self.hi = self.hi.max(v);
    }

    /// -1..1 with a dead zone around the centre.
    fn normalize(&self, v: f32) -> f32 {
        if !self.centered() || self.half <= 0.0 {
            return 0.0;
        }
        let d = v - self.center;
        let floor = MIN_EXTENT * self.half;
        let ext = if d >= 0.0 { (self.hi - self.center).max(floor) } else { (self.center - self.lo).max(floor) };
        let n = (d / ext).clamp(-1.0, 1.0);
        if n.abs() < DEADZONE {
            0.0
        } else {
            n.signum() * (n.abs() - DEADZONE) / (1.0 - DEADZONE)
        }
    }

    /// Whether the (negative, positive) directions have been pushed far enough.
    pub fn calibrated(&self) -> (bool, bool) {
        let floor = MIN_EXTENT * self.half;
        (
            self.centered() && self.center - self.lo >= floor,
            self.centered() && self.hi - self.center >= floor,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JoyState {
    /// -1 (left) .. 1 (right), dead zone applied.
    pub x: f32,
    /// -1 .. 1 in HID orientation (most sticks: pushing forward gives -1).
    pub y: f32,
    pub button: bool,
    pub raw: (i32, i32),
    /// Still averaging the rest position.
    pub centering: bool,
    pub cal_x: (bool, bool),
    pub cal_y: (bool, bool),
}

pub struct Joystick {
    dev: HidDevice,
    layout: Layout,
    cal: [AxisCal; 2],
    pub name: String,
}

impl Joystick {
    /// Open the first HID joystick or gamepad that has X and Y axes.
    pub fn open() -> Result<Self, String> {
        let api = HidApi::new().map_err(|e| e.to_string())?;
        for info in api.device_list() {
            // Generic Desktop / Joystick (4) or Game Pad (5).
            if info.usage_page() != 0x01 || !matches!(info.usage(), 0x04 | 0x05) {
                continue;
            }
            let Ok(dev) = info.open_device(&api) else { continue };
            let mut desc = [0u8; 4096];
            let Ok(n) = dev.get_report_descriptor(&mut desc) else { continue };
            let Some(layout) = parse_descriptor(&desc[..n]) else { continue };
            let name = info.product_string().unwrap_or("Joystick").trim().to_string();
            return Ok(Self { dev, cal: [AxisCal::new(&layout.x), AxisCal::new(&layout.y)], layout, name });
        }
        Err("no joystick found".into())
    }

    /// Forget the calibration; the next reports define the centre again.
    pub fn recalibrate(&mut self) {
        self.cal = [AxisCal::new(&self.layout.x), AxisCal::new(&self.layout.y)];
    }

    /// Read one report. Returns `None` on timeout or for unrelated reports.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<JoyState>, HidError> {
        let mut buf = [0u8; 64];
        let n = self.dev.read_timeout(&mut buf, timeout.as_millis() as i32)?;
        if n == 0 {
            return Ok(None);
        }
        let l = self.layout;
        let data = if l.uses_report_ids {
            if buf[0] != l.x.report_id {
                return Ok(None);
            }
            &buf[1..n]
        } else {
            &buf[..n]
        };
        let (Some(x), Some(y)) = (extract(data, &l.x), extract(data, &l.y)) else { return Ok(None) };
        let button = l
            .button
            .filter(|b| b.report_id == l.x.report_id)
            .and_then(|b| extract(data, &b))
            .is_some_and(|v| v != 0);
        self.cal[0].feed(x as f32);
        self.cal[1].feed(y as f32);
        Ok(Some(JoyState {
            x: self.cal[0].normalize(x as f32),
            y: self.cal[1].normalize(y as f32),
            button,
            raw: (x, y),
            centering: !self.cal[0].centered(),
            cal_x: self.cal[0].calibrated(),
            cal_y: self.cal[1].calibrated(),
        }))
    }
}

fn extract(data: &[u8], f: &Field) -> Option<i32> {
    let mut v: u32 = 0;
    for b in 0..f.bit_size {
        let bit = f.bit_offset + b;
        let byte = *data.get(bit / 8)?;
        v |= (((byte >> (bit % 8)) & 1) as u32) << b;
    }
    if f.logical_min < 0 && f.bit_size < 32 && v & (1 << (f.bit_size - 1)) != 0 {
        v |= !0u32 << f.bit_size; // sign-extend
    }
    Some(v as i32)
}

/// Minimal HID report descriptor parser: enough to locate X, Y and button 1.
fn parse_descriptor(d: &[u8]) -> Option<Layout> {
    let (mut page, mut lmin, mut lmax, mut size, mut count, mut rid) = (0u32, 0i32, 0i32, 0usize, 0usize, 0u8);
    let mut usages: Vec<u32> = vec![];
    let (mut umin, mut umax): (Option<u32>, Option<u32>) = (None, None);
    let mut offsets = std::collections::HashMap::<u8, usize>::new();
    let (mut x, mut y, mut button) = (None, None, None);
    let mut uses_ids = false;

    let mut i = 0;
    while i < d.len() {
        let prefix = d[i];
        if prefix == 0xFE {
            i += 3 + *d.get(i + 1)? as usize; // long item
            continue;
        }
        let len = [0, 1, 2, 4][(prefix & 3) as usize];
        let bytes = d.get(i + 1..i + 1 + len)?;
        i += 1 + len;
        let u = bytes.iter().rev().fold(0u32, |a, &b| (a << 8) | b as u32);
        let s = match len {
            1 => u as u8 as i8 as i32,
            2 => u as u16 as i16 as i32,
            _ => u as i32,
        };
        let full_usage = |u: u32| if len == 4 { u } else { (page << 16) | u };
        match (prefix >> 2) & 3 {
            0 => {
                // Main item.
                match prefix >> 4 {
                    0x8 => {
                        let off = offsets.entry(rid).or_insert(0);
                        let constant = u & 1 != 0;
                        for j in 0..count {
                            let usage = if !usages.is_empty() {
                                Some(usages[j.min(usages.len() - 1)])
                            } else {
                                umin.map(|m| m + j as u32).filter(|v| umax.is_none_or(|mx| *v <= mx))
                            };
                            let field = Field {
                                report_id: rid,
                                bit_offset: *off,
                                bit_size: size,
                                logical_min: lmin,
                                logical_max: lmax,
                            };
                            match usage.filter(|_| !constant) {
                                Some(0x0001_0030) if x.is_none() => x = Some(field),
                                Some(0x0001_0031) if y.is_none() => y = Some(field),
                                Some(0x0009_0001) if button.is_none() => button = Some(field),
                                _ => {}
                            }
                            *off += size;
                        }
                    }
                    _ => {} // output, feature, collections
                }
                usages.clear();
                umin = None;
                umax = None;
            }
            1 => match prefix >> 4 {
                0x0 => page = u,
                0x1 => lmin = s,
                0x2 => lmax = if lmin >= 0 && s < lmin { u as i32 } else { s },
                0x7 => size = u as usize,
                0x8 => {
                    rid = u as u8;
                    uses_ids = true;
                }
                0x9 => count = u as usize,
                _ => {}
            },
            2 => match prefix >> 4 {
                0x0 => usages.push(full_usage(u)),
                0x1 => umin = Some(full_usage(u)),
                0x2 => umax = Some(full_usage(u)),
                _ => {}
            },
            _ => {}
        }
    }
    let (x, y) = (x?, y?);
    (x.report_id == y.report_id).then_some(Layout { x, y, button, uses_report_ids: uses_ids })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SparkFun Pro Micro running the Arduino Joystick library.
    const PRO_MICRO: &str = "05 01 09 04 a1 01 85 03 05 09 19 01 29 01 15 00 25 01 75 01 95 01 55 00 65 00 81 02 75 01 95 07 81 03 05 01 09 01 16 00 00 26 ff 03 75 10 95 02 a1 00 09 30 09 31 81 02 c0 c0";

    fn bytes(s: &str) -> Vec<u8> {
        s.split(' ').map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    #[test]
    fn parses_pro_micro() {
        let l = parse_descriptor(&bytes(PRO_MICRO)).unwrap();
        assert!(l.uses_report_ids);
        assert_eq!((l.x.report_id, l.x.bit_offset, l.x.bit_size, l.x.logical_max), (3, 8, 16, 1023));
        assert_eq!((l.y.bit_offset, l.y.bit_size), (24, 16));
        let b = l.button.unwrap();
        assert_eq!((b.bit_offset, b.bit_size), (0, 1));
        // A real report from the device, without the report ID byte: X=508, Y=511.
        let data = [0u8, 252, 1, 255, 1];
        assert_eq!(extract(&data, &l.x), Some(508));
        assert_eq!(extract(&data, &l.y), Some(511));
        assert_eq!(extract(&[1u8, 0, 0, 0, 0], &b), Some(1));
    }

    #[test]
    fn calibration() {
        let f = Field { report_id: 0, bit_offset: 0, bit_size: 16, logical_min: 0, logical_max: 1023 };
        let mut c = AxisCal::new(&f);
        for _ in 0..CENTER_SAMPLES {
            c.feed(510.0);
        }
        assert_eq!(c.normalize(512.0), 0.0); // dead zone
        // Before calibration a small move stays small (extent = half of 511.5).
        c.feed(640.0);
        assert!((c.normalize(640.0) - 0.465).abs() < 0.01);
        assert_eq!(c.calibrated(), (false, false));
        // A full push sets the real range: hardware tops out at 900.
        c.feed(900.0);
        assert_eq!(c.normalize(900.0), 1.0);
        assert_eq!(c.calibrated(), (false, true));
        c.feed(120.0);
        assert_eq!(c.normalize(120.0), -1.0);
        assert_eq!(c.calibrated(), (true, true));
    }
}

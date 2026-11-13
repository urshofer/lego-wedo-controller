//! Generic USB HID joystick: finds all axes and buttons by parsing the HID
//! report descriptor, and calibrates each axis from what it sees. Which axis
//! and button do what is up to the caller.
//!
//! XInput pads (Xbox 360 style, e.g. a Logitech F310/F710 with the switch on
//! "X") are not HID devices, so they are read directly over USB via libusb,
//! with a fixed layout of sticks, triggers and buttons.
//!
//! Calibration: the centre is the average of the first reports after
//! connecting (leave the stick at rest), and each direction's range grows to
//! the furthest point the axis has reached. Until a direction has been pushed
//! at least halfway out, half the logical range is used as its extent, so a
//! small first move never jumps to full power.

use hidapi::{HidApi, HidDevice};
use std::time::Duration;

/// XInput interface: vendor class 0xff, subclass 0x5d, protocol 0x01.
const XINPUT_CLASS: (u8, u8, u8) = (0xff, 0x5d, 0x01);
const XINPUT_IN_EP: u8 = 0x81;

const CENTER_SAMPLES: u32 = 16;
pub const DEFAULT_DEADZONE: f32 = 0.08;
/// A direction counts as calibrated once it reached this share of the half range.
const MIN_EXTENT: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Field {
    report_id: u8,
    bit_offset: usize,
    bit_size: usize,
    logical_min: i32,
    logical_max: i32,
}

#[derive(Debug, Clone)]
struct Axis {
    name: String,
    field: Field,
    /// Negate the value (XInput sticks report up as positive; HID uses down).
    flip: bool,
}

#[derive(Debug, Clone)]
struct Button {
    name: String,
    field: Field,
}

#[derive(Debug, Clone)]
struct Layout {
    axes: Vec<Axis>,
    buttons: Vec<Button>,
    report_id: u8,
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
    min: f32,
}

impl AxisCal {
    fn new(f: &Field) -> Self {
        Self { half: (f.logical_max - f.logical_min) as f32 / 2.0, min: f.logical_min as f32, ..Default::default() }
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
    fn normalize(&self, v: f32, deadzone: f32) -> f32 {
        if !self.centered() || self.half <= 0.0 {
            return 0.0;
        }
        let d = v - self.center;
        let floor = MIN_EXTENT * self.half;
        let ext = if d >= 0.0 { (self.hi - self.center).max(floor) } else { (self.center - self.lo).max(floor) };
        let n = (d / ext).clamp(-1.0, 1.0);
        if n.abs() < deadzone {
            0.0
        } else {
            n.signum() * (n.abs() - deadzone) / (1.0 - deadzone)
        }
    }

    /// Whether the (negative, positive) directions have been pushed far enough.
    /// An axis resting at its minimum (a trigger) has no negative direction.
    pub fn calibrated(&self) -> (bool, bool) {
        let floor = MIN_EXTENT * self.half;
        let one_sided = self.center - self.min < 0.1 * self.half;
        (
            self.centered() && (one_sided || self.center - self.lo >= floor),
            self.centered() && self.hi - self.center >= floor,
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct JoyState {
    /// Per axis -1 .. 1, dead zone applied. Sticks: right and down are positive.
    pub axes: Vec<f32>,
    pub raw: Vec<i32>,
    pub buttons: Vec<bool>,
    /// Still averaging the rest position.
    pub centering: bool,
    /// Per axis: (negative, positive) direction calibrated.
    pub cal: Vec<(bool, bool)>,
}

enum Backend {
    Hid(HidDevice),
    XInput { handle: rusb::DeviceHandle<rusb::GlobalContext>, iface: u8 },
}

pub struct Joystick {
    backend: Backend,
    layout: Layout,
    cal: Vec<AxisCal>,
    pub name: String,
    pub deadzone: f32,
    /// Last report's data. Many pads (all XInput ones) only report changes,
    /// so on a timeout the last report still describes the stick.
    last: Option<Vec<u8>>,
}

impl Joystick {
    /// Open the first HID joystick or gamepad that has an axis, or else the
    /// first XInput pad.
    pub fn open() -> Result<Self, String> {
        Self::open_hid().or_else(|_| Self::open_xinput())
    }

    fn new(backend: Backend, layout: Layout, name: String) -> Self {
        let cal = layout.axes.iter().map(|a| AxisCal::new(&a.field)).collect();
        // XInput pads start neutral: all-zero data is sticks centred, triggers released.
        let last = matches!(backend, Backend::XInput { .. }).then(|| vec![0u8; 20]);
        Self { backend, layout, cal, name, deadzone: DEFAULT_DEADZONE, last }
    }

    fn open_hid() -> Result<Self, String> {
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
            return Ok(Self::new(Backend::Hid(dev), layout, name));
        }
        Err("no joystick found".into())
    }

    fn open_xinput() -> Result<Self, String> {
        let devices = rusb::devices().map_err(|e| e.to_string())?;
        for dev in devices.iter() {
            let Ok(cfg) = dev.config_descriptor(0) else { continue };
            let iface = cfg.interfaces().flat_map(|i| i.descriptors()).find(|d| {
                (d.class_code(), d.sub_class_code(), d.protocol_code()) == XINPUT_CLASS
                    && d.endpoint_descriptors().any(|e| e.address() == XINPUT_IN_EP)
            });
            let Some(iface) = iface.map(|d| d.interface_number()) else { continue };
            let Ok(handle) = dev.open() else { continue };
            let _ = handle.set_auto_detach_kernel_driver(true);
            if handle.active_configuration().ok() != Some(cfg.number()) {
                let _ = handle.set_active_configuration(cfg.number());
            }
            if handle.claim_interface(iface).is_err() {
                continue;
            }
            let name = dev
                .device_descriptor()
                .ok()
                .and_then(|d| handle.read_product_string_ascii(&d).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "XInput gamepad".into());
            return Ok(Self::new(Backend::XInput { handle, iface }, xinput_layout(), name));
        }
        Err("no XInput pad found".into())
    }

    pub fn axis_names(&self) -> Vec<String> {
        self.layout.axes.iter().map(|a| a.name.clone()).collect()
    }

    pub fn button_names(&self) -> Vec<String> {
        self.layout.buttons.iter().map(|b| b.name.clone()).collect()
    }

    /// Forget the calibration; the next reports define the centre again.
    pub fn recalibrate(&mut self) {
        self.cal = self.layout.axes.iter().map(|a| AxisCal::new(&a.field)).collect();
    }

    /// Read one report; on a timeout, repeat the last one. Returns `None` for
    /// unrelated reports, or on a timeout before any report arrived.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<JoyState>, String> {
        let mut buf = [0u8; 64];
        let n = match &self.backend {
            Backend::Hid(dev) => dev.read_timeout(&mut buf, timeout.as_millis() as i32).map_err(|e| e.to_string())?,
            Backend::XInput { handle, .. } => match handle.read_interrupt(XINPUT_IN_EP, &mut buf, timeout) {
                Ok(n) => n,
                Err(rusb::Error::Timeout) => 0,
                Err(e) => return Err(e.to_string()),
            },
        };
        if n == 0 {
            let Some(last) = self.last.take() else { return Ok(None) };
            let st = self.decode(&last);
            self.last = Some(last);
            return Ok(st);
        }
        let data = match self.backend {
            // Only input reports (type 0, length 0x14); skip LED/rumble status.
            Backend::XInput { .. } if n < 14 || buf[0] != 0x00 => return Ok(None),
            Backend::Hid(_) if self.layout.uses_report_ids => {
                if buf[0] != self.layout.report_id {
                    return Ok(None);
                }
                &buf[1..n]
            }
            _ => &buf[..n],
        };
        let st = self.decode(data);
        if st.is_some() {
            self.last = Some(data.to_vec());
        }
        Ok(st)
    }

    fn decode(&mut self, data: &[u8]) -> Option<JoyState> {
        let mut raw = Vec::with_capacity(self.layout.axes.len());
        for a in &self.layout.axes {
            let v = extract(data, &a.field)?;
            raw.push(if a.flip { (-v).min(a.field.logical_max) } else { v });
        }
        let buttons = self.layout.buttons.iter().map(|b| extract(data, &b.field).is_some_and(|v| v != 0)).collect();
        for (c, &v) in self.cal.iter_mut().zip(&raw) {
            c.feed(v as f32);
        }
        Some(JoyState {
            axes: self.cal.iter().zip(&raw).map(|(c, &v)| c.normalize(v as f32, self.deadzone)).collect(),
            raw,
            buttons,
            centering: self.cal.iter().any(|c| !c.centered()),
            cal: self.cal.iter().map(AxisCal::calibrated).collect(),
        })
    }
}

impl Drop for Joystick {
    fn drop(&mut self) {
        if let Backend::XInput { handle, iface } = &self.backend {
            let _ = handle.release_interface(*iface);
        }
    }
}

/// XInput input report: buttons u16 at byte 2, triggers u8 at 4 and 5,
/// sticks as i16 at 6 (LX), 8 (LY), 10 (RX), 12 (RY).
fn xinput_layout() -> Layout {
    let field = |byte: usize, bits: usize, min: i32, max: i32| Field {
        report_id: 0,
        bit_offset: byte * 8,
        bit_size: bits,
        logical_min: min,
        logical_max: max,
    };
    let stick = |name: &str, byte: usize, flip: bool| Axis { name: name.into(), field: field(byte, 16, -32768, 32767), flip };
    let trigger = |name: &str, byte: usize| Axis { name: name.into(), field: field(byte, 8, 0, 255), flip: false };
    let axes = vec![
        stick("Left stick X", 6, false),
        stick("Left stick Y", 8, true),
        stick("Right stick X", 10, false),
        stick("Right stick Y", 12, true),
        trigger("Left trigger", 4),
        trigger("Right trigger", 5),
    ];
    const BUTTONS: [(&str, usize); 15] = [
        ("A", 12), ("B", 13), ("X", 14), ("Y", 15), ("LB", 8), ("RB", 9), ("Back", 5), ("Start", 4),
        ("Guide", 10), ("Left stick press", 6), ("Right stick press", 7),
        ("D-pad up", 0), ("D-pad down", 1), ("D-pad left", 2), ("D-pad right", 3),
    ];
    let buttons = BUTTONS
        .iter()
        .map(|&(name, bit)| Button { name: name.into(), field: Field { bit_offset: 16 + bit, ..field(0, 1, 0, 1) } })
        .collect();
    Layout { axes, buttons, report_id: 0, uses_report_ids: false }
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

fn axis_name(usage: u32) -> Option<&'static str> {
    Some(match usage {
        0x0001_0030 => "X",
        0x0001_0031 => "Y",
        0x0001_0032 => "Z",
        0x0001_0033 => "Rx",
        0x0001_0034 => "Ry",
        0x0001_0035 => "Rz",
        0x0001_0036 => "Slider",
        0x0001_0037 => "Dial",
        0x0001_0038 => "Wheel",
        _ => return None,
    })
}

/// Minimal HID report descriptor parser: collects the axes and buttons of the
/// report that carries the first axis.
fn parse_descriptor(d: &[u8]) -> Option<Layout> {
    let (mut page, mut lmin, mut lmax, mut size, mut count, mut rid) = (0u32, 0i32, 0i32, 0usize, 0usize, 0u8);
    let mut usages: Vec<u32> = vec![];
    let (mut umin, mut umax): (Option<u32>, Option<u32>) = (None, None);
    let mut offsets = std::collections::HashMap::<u8, usize>::new();
    let (mut axes, mut buttons): (Vec<Axis>, Vec<Button>) = (vec![], vec![]);
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
                // Main item: only inputs (0x8) carry data.
                if prefix >> 4 == 0x8 {
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
                        *off += size;
                        let Some(usage) = usage.filter(|_| !constant) else { continue };
                        if let Some(base) = axis_name(usage) {
                            // Second slider etc. gets a number.
                            let same = axes.iter().filter(|a| a.name.starts_with(base)).count();
                            let name = if same == 0 { base.to_string() } else { format!("{base} {}", same + 1) };
                            axes.push(Axis { name, field, flip: false });
                        } else if usage >> 16 == 0x0009 && size == 1 {
                            buttons.push(Button { name: format!("Button {}", usage & 0xffff), field });
                        }
                    }
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
    let report_id = axes.first()?.field.report_id;
    axes.retain(|a| a.field.report_id == report_id);
    buttons.retain(|b| b.field.report_id == report_id);
    Some(Layout { axes, buttons, report_id, uses_report_ids: uses_ids })
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
        assert_eq!(l.report_id, 3);
        let names: Vec<_> = l.axes.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["X", "Y"]);
        let (x, y) = (l.axes[0].field, l.axes[1].field);
        assert_eq!((x.bit_offset, x.bit_size, x.logical_max), (8, 16, 1023));
        assert_eq!((y.bit_offset, y.bit_size), (24, 16));
        assert_eq!(l.buttons.len(), 1);
        assert_eq!(l.buttons[0].name, "Button 1");
        let b = l.buttons[0].field;
        assert_eq!((b.bit_offset, b.bit_size), (0, 1));
        // A real report from the device, without the report ID byte: X=508, Y=511.
        let data = [0u8, 252, 1, 255, 1];
        assert_eq!(extract(&data, &x), Some(508));
        assert_eq!(extract(&data, &y), Some(511));
        assert_eq!(extract(&[1u8, 0, 0, 0, 0], &b), Some(1));
    }

    #[test]
    fn xinput_report() {
        let l = xinput_layout();
        // Captured from a Logitech F310: LX = 0xf074 (-3980), LY = 0xf373, A pressed.
        let r = [0x00u8, 0x14, 0x00, 0x10, 0x28, 0x29, 0x74, 0xf0, 0x73, 0xf3, 0x76, 0xf6, 0x76, 0xf6];
        assert_eq!(extract(&r, &l.axes[0].field), Some(-3980));
        assert_eq!(extract(&r, &l.axes[4].field), Some(0x28));
        let pressed: Vec<_> = l.buttons.iter().filter(|b| extract(&r, &b.field) == Some(1)).map(|b| &b.name).collect();
        assert_eq!(pressed, ["A"]);
    }

    #[test]
    fn calibration() {
        let f = Field { report_id: 0, bit_offset: 0, bit_size: 16, logical_min: 0, logical_max: 1023 };
        let mut c = AxisCal::new(&f);
        for _ in 0..CENTER_SAMPLES {
            c.feed(510.0);
        }
        let dz = DEFAULT_DEADZONE;
        assert_eq!(c.normalize(512.0, dz), 0.0); // dead zone
        // Before calibration a small move stays small (extent = half of 511.5).
        c.feed(640.0);
        assert!((c.normalize(640.0, dz) - 0.465).abs() < 0.01);
        assert_eq!(c.calibrated(), (false, false));
        // A full push sets the real range: hardware tops out at 900.
        c.feed(900.0);
        assert_eq!(c.normalize(900.0, dz), 1.0);
        assert_eq!(c.calibrated(), (false, true));
        c.feed(120.0);
        assert_eq!(c.normalize(120.0, dz), -1.0);
        assert_eq!(c.calibrated(), (true, true));
    }

    #[test]
    fn trigger_is_one_sided() {
        let f = Field { report_id: 0, bit_offset: 0, bit_size: 8, logical_min: 0, logical_max: 255 };
        let mut c = AxisCal::new(&f);
        for _ in 0..CENTER_SAMPLES {
            c.feed(0.0);
        }
        assert_eq!(c.calibrated(), (true, false));
        c.feed(255.0);
        assert_eq!(c.normalize(255.0, 0.0), 1.0);
        assert_eq!(c.calibrated(), (true, true));
    }
}

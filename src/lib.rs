//! Driver for the LEGO WeDo 1.0 USB hub (sets 9580 / 9581 / 45303).
//!
//! The hub is a plain USB HID device. The host sends an 8-byte output
//! report with the two port values, and the hub streams 8-byte input
//! reports carrying an ID (set by a resistor in the plugged-in device)
//! and a raw value for each port.
//!
//! The ID table follows the ev3dev Linux driver (`wedo/wedo_port.c`).

pub mod joystick;

use hidapi::{HidApi, HidDevice, HidError};
use std::time::{Duration, Instant};

pub const VENDOR_ID: u16 = 0x0694; // LEGO
pub const PRODUCT_ID: u16 = 0x0003; // WeDo 1.0 hub

/// How many consecutive reports must agree before a port's device type
/// changes. The ID reading is noisy while plugging and while motors run.
const DEBOUNCE: u8 = 8;

/// While a port is driven its ID line follows the output (forward reads ~3,
/// reverse ~254, and speed changes sweep through every range), so the device
/// type is frozen while driving and for this long after the output returns to 0.
const SETTLE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    A,
    B,
}

impl Port {
    fn index(self) -> usize {
        match self {
            Port::A => 0,
            Port::B => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tilt {
    Level,
    Forward,
    Back,
    Left,
    Right,
}

/// Device class, decoded from the port's ID byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// ID 0-9 or 247-255: short circuit (also seen briefly while a motor starts).
    Short,
    Bend,
    Tilt,
    Future,
    Raw,
    Touch,
    /// Power Functions servo motor (88004).
    Servo,
    Sound,
    Temperature,
    Light,
    /// The WeDo motion (distance) sensor.
    Motion,
    /// LEGO light brick / Power Functions LED lights.
    LightBrick,
    Reserved22,
    /// Nothing plugged in.
    Empty,
    /// WeDo / Power Functions motor.
    Motor,
}

impl Kind {
    pub fn from_id(id: u8) -> Kind {
        match id {
            0..=9 => Kind::Short,
            10..=27 => Kind::Bend,
            28..=47 => Kind::Tilt,
            48..=67 => Kind::Future,
            68..=87 => Kind::Raw,
            88..=100 => Kind::Touch,
            101..=109 => Kind::Servo,
            110..=131 => Kind::Sound,
            132..=152 => Kind::Temperature,
            153..=169 => Kind::Light,
            170..=190 => Kind::Motion,
            191..=211 => Kind::LightBrick,
            212..=224 => Kind::Reserved22,
            225..=233 => Kind::Empty,
            234..=246 => Kind::Motor,
            247..=255 => Kind::Short,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Short => "Short circuit",
            Kind::Bend => "Bend sensor",
            Kind::Tilt => "Tilt sensor",
            Kind::Future => "Future device",
            Kind::Raw => "Raw input",
            Kind::Touch => "Touch sensor",
            Kind::Servo => "Servo motor",
            Kind::Sound => "Sound sensor",
            Kind::Temperature => "Temperature sensor",
            Kind::Light => "Light sensor",
            Kind::Motion => "Motion sensor",
            Kind::LightBrick => "Light brick",
            Kind::Reserved22 => "Reserved (22)",
            Kind::Empty => "Empty",
            Kind::Motor => "Motor",
        }
    }

    /// Devices driven by the output byte rather than read from.
    pub fn is_output(self) -> bool {
        matches!(self, Kind::Motor | Kind::Servo | Kind::LightBrick)
    }
}

/// What the hub reports is plugged into a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attached {
    Motor,
    Servo,
    LightBrick,
    Tilt(Tilt),
    /// Motion sensor: raw value plus proximity 0-100 % (ev3dev calibration).
    Distance {
        raw: u8,
        percent: u8,
    },
    /// Any other sensor class; only the raw 0-255 value is known.
    Sensor {
        kind: Kind,
        value: u8,
    },
    Empty,
    Short,
}

impl Attached {
    pub fn kind(self) -> Kind {
        match self {
            Attached::Motor => Kind::Motor,
            Attached::Servo => Kind::Servo,
            Attached::LightBrick => Kind::LightBrick,
            Attached::Tilt(_) => Kind::Tilt,
            Attached::Distance { .. } => Kind::Motion,
            Attached::Sensor { kind, .. } => kind,
            Attached::Empty => Kind::Empty,
            Attached::Short => Kind::Short,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub a: Attached,
    pub b: Attached,
    /// Undecoded (id, value) for ports A and B, for calibration.
    pub raw: [(u8, u8); 2],
}

#[derive(Default, Clone, Copy)]
struct Debounce {
    current: Option<Kind>,
    pending: Option<Kind>,
    count: u8,
}

impl Debounce {
    fn feed(&mut self, k: Kind) -> Kind {
        // A running motor pulls the ID line low (WeDoMore lists 0-3 as motor IDs).
        if self.current == Some(Kind::Motor) && k == Kind::Short {
            return Kind::Motor;
        }
        match self.current {
            // Nothing committed yet: still wait for agreeing reports (the first
            // few after opening the hub are noisy).
            None => {
                if self.pending == Some(k) {
                    self.count += 1;
                } else {
                    self.pending = Some(k);
                    self.count = 1;
                }
                if self.count >= DEBOUNCE {
                    self.current = Some(k);
                    self.pending = None;
                }
                return self.current.unwrap_or(Kind::Empty);
            }
            Some(c) if c == k => self.pending = None,
            Some(_) => {
                if self.pending == Some(k) {
                    self.count += 1;
                    if self.count >= DEBOUNCE {
                        self.current = Some(k);
                        self.pending = None;
                    }
                } else {
                    self.pending = Some(k);
                    self.count = 1;
                }
            }
        }
        self.current.unwrap()
    }
}

pub struct WeDo {
    dev: HidDevice,
    out: [i8; 2],
    debounce: [Debounce; 2],
    /// When each port's output last went back to 0 (start of its settle time).
    released: [Option<Instant>; 2],
}

impl WeDo {
    /// Open the first WeDo hub found.
    pub fn open() -> Result<Self, HidError> {
        let api = HidApi::new()?;
        let dev = api.open(VENDOR_ID, PRODUCT_ID)?;
        Ok(Self {
            dev,
            out: [0, 0],
            debounce: Default::default(),
            released: [None; 2],
        })
    }

    /// Set a motor's speed from -100 (full reverse) to 100 (full forward).
    pub fn set_motor(&mut self, port: Port, speed: i32) -> Result<(), HidError> {
        self.set_raw(port, scale_speed(speed))
    }

    /// Set a Power Functions servo position from -100 (full left) to 100 (full right).
    pub fn set_servo(&mut self, port: Port, position: i32) -> Result<(), HidError> {
        self.set_raw(port, (position.clamp(-100, 100) * 127 / 100) as i8)
    }

    /// Set a light brick's brightness from 0 to 100 %.
    pub fn set_light(&mut self, port: Port, brightness: i32) -> Result<(), HidError> {
        self.set_raw(port, (brightness.clamp(0, 100) * 127 / 100) as i8)
    }

    /// Write the port's output byte directly (-127..127).
    pub fn set_raw(&mut self, port: Port, value: i8) -> Result<(), HidError> {
        let i = port.index();
        if self.out[i] != 0 && value == 0 {
            self.released[i] = Some(Instant::now());
        }
        self.out[i] = value;
        self.send()
    }

    pub fn stop(&mut self) -> Result<(), HidError> {
        for i in 0..2 {
            if self.out[i] != 0 {
                self.released[i] = Some(Instant::now());
            }
        }
        self.out = [0, 0];
        self.send()
    }

    fn send(&self) -> Result<(), HidError> {
        // Leading 0x00 is the HID report ID (the hub uses unnumbered reports).
        let report = [
            0x00,
            0x40,
            self.out[0] as u8,
            self.out[1] as u8,
            0,
            0,
            0,
            0,
            0,
        ];
        self.dev.write(&report)?;
        Ok(())
    }

    /// Read the latest sensor report. Returns `None` on timeout.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<Reading>, HidError> {
        let mut buf = [0u8; 8];
        let n = self
            .dev
            .read_timeout(&mut buf, timeout.as_millis() as i32)?;
        if n < 6 {
            return Ok(None);
        }
        let raw = [(buf[3], buf[2]), (buf[5], buf[4])];
        let [ka, kb] = [0, 1].map(|i| {
            let d = &mut self.debounce[i];
            let settling = self.released[i].is_some_and(|t| t.elapsed() < SETTLE);
            match d.current {
                Some(k) if self.out[i] != 0 || settling => {
                    d.pending = None;
                    k
                }
                _ => d.feed(Kind::from_id(raw[i].0)),
            }
        });
        Ok(Some(Reading {
            a: decode(ka, raw[0].1),
            b: decode(kb, raw[1].1),
            raw,
        }))
    }
}

impl Drop for WeDo {
    /// The hub keeps running the last output it was given, so stop on exit.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Map -100..=100 to the hub's signed byte. Small values are pushed up
/// past ~27 because the motor doesn't turn at very low power.
fn scale_speed(speed: i32) -> i8 {
    let s = speed.clamp(-100, 100);
    match s {
        0 => 0,
        s if s > 0 => (27 + s) as i8,
        s => (s - 27) as i8,
    }
}

/// Decode a single report without debouncing.
pub fn classify(id: u8, value: u8) -> Attached {
    decode(Kind::from_id(id), value)
}

fn decode(kind: Kind, value: u8) -> Attached {
    match kind {
        Kind::Motor => Attached::Motor,
        Kind::Servo => Attached::Servo,
        Kind::LightBrick => Attached::LightBrick,
        Kind::Tilt => Attached::Tilt(tilt_from_raw(value)),
        Kind::Motion => Attached::Distance {
            raw: value,
            percent: proximity(value),
        },
        Kind::Empty => Attached::Empty,
        Kind::Short => Attached::Short,
        kind => Attached::Sensor { kind, value },
    }
}

/// ev3dev's PROX calibration: raw 71..219 maps to 0..100 %.
fn proximity(raw: u8) -> u8 {
    ((raw.clamp(71, 219) as u32 - 71) * 100 / (219 - 71)) as u8
}

/// Thresholds as in WeDoMore / ev3dev. The two disagree on which side is
/// "forward"; flip the arms here if your sensor reads mirrored.
fn tilt_from_raw(v: u8) -> Tilt {
    match v {
        0..=48 => Tilt::Forward,
        49..=99 => Tilt::Left,
        100..=153 => Tilt::Level,
        154..=204 => Tilt::Back,
        _ => Tilt::Right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_scaling() {
        assert_eq!(scale_speed(0), 0);
        assert_eq!(scale_speed(100), 127);
        assert_eq!(scale_speed(-100), -127);
        assert_eq!(scale_speed(500), 127);
        assert_eq!(scale_speed(1), 28);
    }

    #[test]
    fn classification() {
        assert_eq!(classify(38, 120), Attached::Tilt(Tilt::Level));
        assert_eq!(
            classify(177, 71),
            Attached::Distance {
                raw: 71,
                percent: 0
            }
        );
        assert_eq!(
            classify(177, 219),
            Attached::Distance {
                raw: 219,
                percent: 100
            }
        );
        assert_eq!(classify(239, 0), Attached::Motor);
        assert_eq!(classify(105, 0), Attached::Servo);
        assert_eq!(classify(200, 0), Attached::LightBrick);
        assert_eq!(classify(230, 0), Attached::Empty);
        assert_eq!(
            classify(95, 7),
            Attached::Sensor {
                kind: Kind::Touch,
                value: 7
            }
        );
        assert_eq!(classify(2, 0), Attached::Short);
    }

    #[test]
    fn every_id_maps() {
        for id in 0..=255u8 {
            let _ = Kind::from_id(id);
        }
    }

    #[test]
    fn debounce_ignores_glitches() {
        let mut d = Debounce::default();
        assert_eq!(d.feed(Kind::Future), Kind::Empty); // startup noise is not committed
        for _ in 0..DEBOUNCE - 1 {
            assert_eq!(d.feed(Kind::Motor), Kind::Empty);
        }
        assert_eq!(d.feed(Kind::Motor), Kind::Motor);
        assert_eq!(d.feed(Kind::Short), Kind::Motor);
        assert_eq!(d.feed(Kind::Motor), Kind::Motor);
        for _ in 0..DEBOUNCE - 1 {
            assert_eq!(d.feed(Kind::Empty), Kind::Motor);
        }
        assert_eq!(d.feed(Kind::Empty), Kind::Empty);
    }
}

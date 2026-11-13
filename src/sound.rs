//! Sounds: short synthesized tones for tilt sensor changes, and a continuous
//! voice per output: a motor plays a looped crane recording that revs up and
//! down with its speed, a
//! servo whines like a gearbox while it travels. A thread owns the audio
//! output; if there is no output device everything is silently dropped.

use rodio::buffer::SamplesBuffer;
use rodio::{ChannelCount, Sample, SampleRate, Source};
use std::f32::consts::TAU;
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use wedo::{Port, Tilt};

const RATE: u32 = 44_100;
const DT: f32 = 1.0 / RATE as f32;
const VOLUME: f32 = 0.25;

/// What a port's output is, for picking its sound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Voice {
    None = 0,
    Motor = 1,
    Servo = 2,
}

/// The output state the synth follows. Written by the hub worker.
#[derive(Default)]
pub struct Outputs {
    voice: [AtomicU8; 2],
    /// -100..100: motor speed or servo position.
    value: [AtomicI32; 2],
    pub enabled: AtomicBool,
}

impl Outputs {
    pub fn set_voice(&self, p: Port, v: Voice) {
        self.voice[p as usize].store(v as u8, Ordering::Relaxed);
    }

    pub fn set_value(&self, p: Port, v: i32) {
        self.value[p as usize].store(v.clamp(-100, 100), Ordering::Relaxed);
    }

    /// Hub gone or all stopped: everything winds down to rest.
    pub fn stop_all(&self) {
        for p in [Port::A, Port::B] {
            self.set_value(p, 0);
        }
    }

    fn get(&self, i: usize) -> (Voice, f32) {
        let v = match self.voice[i].load(Ordering::Relaxed) {
            1 => Voice::Motor,
            2 => Voice::Servo,
            _ => Voice::None,
        };
        (v, self.value[i].load(Ordering::Relaxed) as f32 / 100.0)
    }
}

pub struct Sound {
    pub tilt: Sender<Tilt>,
    pub outputs: Arc<Outputs>,
}

/// Start the audio thread.
pub fn start() -> Sound {
    let (tx, rx) = mpsc::channel::<Tilt>();
    let outputs = Arc::new(Outputs { enabled: AtomicBool::new(true), ..Default::default() });
    let o = outputs.clone();
    thread::spawn(move || {
        let Ok(mut sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
            for _ in rx {} // no audio device: drain and ignore
            return;
        };
        sink.log_on_drop(false);
        sink.mixer().add(Synth::new(o));
        let (channels, rate) = (NonZero::new(1).unwrap(), NonZero::new(RATE).unwrap());
        for t in rx {
            sink.mixer().add(SamplesBuffer::new(channels, rate, tone(t)));
        }
    });
    Sound { tilt: tx, outputs }
}

/// Forward rises, back falls, left is low, right is high, level is a soft tick.
fn tone(t: Tilt) -> Vec<f32> {
    match t {
        Tilt::Forward => sweep(440.0, 880.0, 0.18, 1.0),
        Tilt::Back => sweep(660.0, 330.0, 0.18, 1.0),
        Tilt::Left => [sweep(392.0, 392.0, 0.08, 1.0), sweep(392.0, 392.0, 0.08, 1.0)].concat(),
        Tilt::Right => [sweep(784.0, 784.0, 0.08, 1.0), sweep(784.0, 784.0, 0.08, 1.0)].concat(),
        Tilt::Level => sweep(523.0, 523.0, 0.06, 0.4),
    }
}

/// Sine sweeping linearly from `f0` to `f1` Hz, with a short attack and a
/// linear release so it doesn't click.
fn sweep(f0: f32, f1: f32, secs: f32, gain: f32) -> Vec<f32> {
    let n = (secs * RATE as f32) as usize;
    let attack = (0.005 * RATE as f32) as usize;
    let mut phase = 0.0f32;
    (0..n)
        .map(|i| {
            let p = i as f32 / n as f32;
            phase += TAU * (f0 + (f1 - f0) * p) / RATE as f32;
            let env = (i as f32 / attack as f32).min(1.0) * (1.0 - p);
            phase.sin() * env * gain * VOLUME
        })
        .collect()
}

/// Exponential approach of `x` to `target` with time constant `tau` seconds.
fn approach(x: f32, target: f32, tau: f32) -> f32 {
    x + (target - x) * (DT / tau).min(1.0)
}

/// Advance a 0..1 phase by `freq` Hz.
fn step(phase: &mut f32, freq: f32) -> f32 {
    *phase = (*phase + freq * DT).fract();
    *phase
}

/// Recording of a 1946 coal crane (Bochum-Dahlhausen Railway Museum) by
/// Konrad Gutkowski / Work With Sounds, CC BY 4.0 (see README). 15 s, looped
/// seamlessly; mono 16-bit little-endian PCM at `CRANE_RATE`.
static CRANE: &[u8] = include_bytes!("../assets/crane.raw");
const CRANE_RATE: f32 = 22_050.0;

/// Motor: the crane recording, looped. Speed sets the volume and playback
/// rate (the machine winds up from a low growl to its real pitch); hoisting
/// (forward) runs a little lower and more strained than lowering. Heavy, so
/// it spins up and down slowly.
struct Motor {
    rpm: f32,
    gain: f32,
    /// -1 lowering .. 1 hoisting, follows the last direction driven.
    dir: f32,
    /// Read position in samples.
    pos: f32,
}

impl Default for Motor {
    fn default() -> Self {
        Self { rpm: 0.0, gain: 0.0, dir: 1.0, pos: 0.0 }
    }
}

fn crane_sample(i: usize) -> f32 {
    let n = CRANE.len() / 2;
    let i = (i % n) * 2;
    i16::from_le_bytes([CRANE[i], CRANE[i + 1]]) as f32 / 32768.0
}

impl Motor {
    fn next(&mut self, speed: Option<f32>) -> f32 {
        let speed = speed.unwrap_or(0.0);
        let target = speed.abs();
        if target > 0.05 {
            self.dir = approach(self.dir, speed.signum(), 0.3);
        }
        let tau = if target > self.rpm { 0.6 } else { 0.9 };
        self.rpm = approach(self.rpm, target, tau);
        let audible = if self.rpm > 0.02 { self.rpm.sqrt() } else { 0.0 };
        self.gain = approach(self.gain, audible, 0.03);
        if self.gain < 1e-4 {
            return 0.0;
        }
        let strain = 1.0 - 0.06 * self.dir;
        let rate = (0.55 + 0.45 * self.rpm) * strain * CRANE_RATE / RATE as f32;
        let n = (CRANE.len() / 2) as f32;
        self.pos = (self.pos + rate) % n;
        // Linear interpolation between neighbouring samples.
        let (i, frac) = (self.pos as usize, self.pos.fract());
        let v = crane_sample(i) * (1.0 - frac) + crane_sample(i + 1) * frac;
        v * 2.0 * self.gain
    }
}

/// Servo: the horn travels at a fixed speed towards its target; while it
/// moves, a gear buzz plays, lower going left and higher going right.
#[derive(Default)]
struct Servo {
    pos: Option<f32>,
    gain: f32,
    dir: f32,
    phase: [f32; 3],
}

impl Servo {
    /// Full travel per second (-1..1 is a range of 2).
    const SPEED: f32 = 2.5;

    fn next(&mut self, target: Option<f32>) -> f32 {
        let moving = match (target, self.pos) {
            (Some(t), Some(p)) => {
                let d = t - p;
                let s = Self::SPEED * DT;
                self.pos = Some(if d.abs() <= s { t } else { p + s * d.signum() });
                if d.abs() > s {
                    self.dir = d.signum();
                }
                d.abs() > 1e-3
            }
            (Some(t), None) => {
                self.pos = Some(t); // just plugged in: no sound for the initial position
                false
            }
            (None, _) => {
                self.pos = None;
                false
            }
        };
        self.gain = approach(self.gain, if moving { 1.0 } else { 0.0 }, if moving { 0.008 } else { 0.03 });
        if self.gain < 1e-4 {
            return 0.0;
        }
        let base = if self.dir < 0.0 { 110.0 } else { 150.0 };
        let buzz = if step(&mut self.phase[0], base) < 0.5 { 1.0 } else { -1.0 };
        let whine = (TAU * step(&mut self.phase[1], base * 13.0)).sin();
        // Gear teeth: short ticks 40 times a second.
        let tooth = step(&mut self.phase[2], 40.0);
        let tick = 0.6 + 0.4 * (-tooth * 12.0).exp();
        (0.35 * buzz + 0.25 * whine) * tick * self.gain
    }
}

/// Endless source mixing a motor and a servo voice per port.
struct Synth {
    outputs: Arc<Outputs>,
    motors: [Motor; 2],
    servos: [Servo; 2],
    master: f32,
}

impl Synth {
    fn new(outputs: Arc<Outputs>) -> Self {
        Self { outputs, motors: Default::default(), servos: Default::default(), master: 0.0 }
    }
}

impl Iterator for Synth {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let on = self.outputs.enabled.load(Ordering::Relaxed);
        self.master = approach(self.master, if on { 1.0 } else { 0.0 }, 0.02);
        let mut out = 0.0;
        for i in 0..2 {
            let (voice, v) = self.outputs.get(i);
            out += self.motors[i].next((voice == Voice::Motor).then_some(v));
            out += self.servos[i].next((voice == Voice::Servo).then_some(v));
        }
        Some((out * 0.5 * self.master).tanh() * VOLUME)
    }
}

impl Source for Synth {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        NonZero::new(1).unwrap()
    }
    fn sample_rate(&self) -> SampleRate {
        NonZero::new(RATE).unwrap()
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(mut f: impl FnMut() -> f32, secs: f32) -> f32 {
        let n = (secs * RATE as f32) as usize;
        ((0..n).map(|_| f().powi(2)).sum::<f32>() / n as f32).sqrt()
    }

    #[test]
    fn motor_revs_up_and_down() {
        let mut m = Motor::default();
        assert_eq!(rms(|| m.next(Some(0.0)), 0.1), 0.0);
        rms(|| m.next(Some(1.0)), 0.1);
        let mid = m.rpm;
        assert!(mid > 0.1 && mid < 0.5, "still revving up: {mid}");
        let loud = rms(|| m.next(Some(-1.0)), 2.5); // reverse revs just as high; heavy, so slow
        assert!(m.rpm > 0.95 && loud > 0.05);
        let mut peak = 0.0f32;
        rms(|| { let v = m.next(Some(1.0)); peak = peak.max(v.abs()); v }, 1.0);
        assert!(peak < 1.5, "two ports mixed stay well within tanh range: {peak}");
        rms(|| m.next(Some(0.0)), 0.3);
        assert!(m.rpm > 0.3, "winds down gradually: {}", m.rpm);
        rms(|| m.next(Some(0.0)), 5.0);
        assert_eq!(rms(|| m.next(Some(0.0)), 0.1), 0.0);
    }

    #[test]
    fn servo_sounds_only_while_moving() {
        let mut s = Servo::default();
        assert_eq!(rms(|| s.next(Some(0.5)), 0.2), 0.0); // plugged in: silent
        assert!(rms(|| s.next(Some(-1.0)), 0.3) > 0.05); // 1.5 travel takes 0.6 s
        assert!(s.dir < 0.0);
        rms(|| s.next(Some(-1.0)), 0.5);
        assert_eq!(s.pos, Some(-1.0));
        assert!(rms(|| s.next(Some(-1.0)), 0.2) < 1e-3);
    }
}

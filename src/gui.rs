//! Desktop frontend. A worker thread owns the hub, reconnects when it is
//! unplugged, and publishes the latest readings; the UI only sends commands.
//! A second thread reads a USB joystick: X drives port A, Y drives port B,
//! and its button is a latching emergency stop.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Stroke, Vec2};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use crate::sound::{Outputs, Sound, Voice};
use crate::joyconfig::JoyConfig;
use wedo::joystick::{JoyState, Joystick};
use wedo::{Attached, Kind, Port, Reading, Tilt, WeDo};

const GREEN: Color32 = Color32::from_rgb(46, 160, 67);
const RED: Color32 = Color32::from_rgb(207, 70, 60);
const ACCENT: Color32 = Color32::from_rgb(240, 180, 30);
/// A tilt direction must be read this many times in a row before it sounds.
const TILT_STABLE: u32 = 3;
/// A port's device must read differently this many times in a row before we
/// believe it changed: an attached motor's ID flickers.
const KIND_STABLE: u32 = 10;
/// Motor speed when the tilt sensor drives it.
const TILT_SPEED: i32 = 100;

#[derive(Clone, Copy, PartialEq)]
enum Output {
    Motor,
    Servo,
    Light,
}

impl Output {
    fn for_kind(k: Option<Kind>) -> Output {
        match k {
            Some(Kind::Servo) => Output::Servo,
            Some(Kind::LightBrick) => Output::Light,
            _ => Output::Motor,
        }
    }
}

enum Cmd {
    Set(Port, Output, i32),
    /// From the joystick: -100..100, applied as whatever the port has attached.
    Drive(Port, i32),
    StopAll,
    /// Latch (true) or release (false) the emergency stop.
    EStop(bool),
}

struct JoyShared {
    connected: bool,
    name: String,
    state: Option<JoyState>,
    axis_names: Vec<String>,
    button_names: Vec<String>,
    config: JoyConfig,
    /// Waiting for the user to move an axis or press a button to assign it.
    learn: Option<Learn>,
    /// Mapped stick per port, -1..1 after invert, before the speed limit.
    stick: [f32; 2],
    estop_pressed: bool,
    /// Emergency stop latched: all outputs ignored until re-armed.
    estop: bool,
    /// Last values the joystick sent, and a counter so the UI notices new ones.
    drive: [i32; 2],
    generation: u64,
    recalibrate: bool,
    save_error: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Learn {
    /// Axis for port 0 (A) or 1 (B).
    Axis(usize),
    EStop,
}

impl Default for JoyShared {
    fn default() -> Self {
        Self {
            connected: false,
            name: String::new(),
            state: None,
            axis_names: vec![],
            button_names: vec![],
            config: JoyConfig::default_for(0, 0),
            learn: None,
            stick: [0.0; 2],
            estop_pressed: false,
            estop: false,
            drive: [0, 0],
            generation: 0,
            recalibrate: false,
            save_error: None,
        }
    }
}

impl JoyShared {
    fn save(&mut self) {
        self.save_error = self.config.save(&self.name, &self.axis_names, &self.button_names).err().map(|e| e.to_string());
    }

    fn axis_name(&self, i: Option<usize>) -> &str {
        i.and_then(|i| self.axis_names.get(i)).map_or("—", String::as_str)
    }

    fn button_name(&self, i: Option<usize>) -> &str {
        i.and_then(|i| self.button_names.get(i)).map_or("—", String::as_str)
    }
}

#[derive(Default)]
struct Shared {
    connected: bool,
    reading: Option<Reading>,
    /// Ports currently driven by the tilt sensor, and their value.
    linked: [Option<i32>; 2],
}

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WeDo Controller")
            .with_inner_size([700.0, 720.0])
            .with_min_inner_size([520.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "WeDo Controller",
        options,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}

struct App {
    shared: Arc<Mutex<Shared>>,
    joy: Arc<Mutex<JoyShared>>,
    joy_generation: u64,
    joy_config_open: bool,
    tx: Sender<Cmd>,
    speed: [i32; 2],
    mode: [Output; 2],
    tilt_sound: Arc<AtomicBool>,
    tilt_link: Arc<AtomicBool>,
    outputs: Arc<Outputs>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        let tilt_sound = Arc::new(AtomicBool::new(true));
        let sound = crate::sound::start();
        let outputs = sound.outputs.clone();
        let tilt_link = Arc::new(AtomicBool::new(true));
        let (s, c, on, link) = (shared.clone(), ctx.clone(), tilt_sound.clone(), tilt_link.clone());
        thread::spawn(move || worker(s, rx, c, sound, on, link));
        let joy = Arc::new(Mutex::new(JoyShared::default()));
        let (j, t, c) = (joy.clone(), tx.clone(), ctx.clone());
        thread::spawn(move || joystick_worker(j, t, c));
        Self {
            shared,
            joy,
            joy_generation: 0,
            joy_config_open: false,
            tx,
            speed: [0, 0],
            mode: [Output::Motor; 2],
            tilt_sound,
            tilt_link,
            outputs,
        }
    }
}

/// Owns the hub. Retries the connection every second while unplugged.
fn worker(
    shared: Arc<Mutex<Shared>>,
    rx: Receiver<Cmd>,
    ctx: egui::Context,
    sound: Sound,
    tilt_sound: Arc<AtomicBool>,
    tilt_link: Arc<AtomicBool>,
) {
    let out = &sound.outputs;
    let mut estop = false;
    loop {
        let mut hub = match WeDo::open() {
            Ok(h) => h,
            Err(_) => {
                // Drain commands so they don't fire all at once on reconnect.
                loop {
                    match rx.try_recv() {
                        Ok(Cmd::EStop(on)) => estop = on,
                        Ok(_) => {}
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        set(&shared, &ctx, true, None);
        let mut kinds: [Option<Kind>; 2] = [None, None];
        let mut tilts = [TiltTracker::default(); 2];
        let mut devices = [KindTracker::default(); 2];
        // Ports the tilt sensor drives, with the value last sent.
        let mut linked: [Option<i32>; 2] = [None, None];

        'connected: loop {
            loop {
                let cmd = match rx.try_recv() {
                    Ok(c) => c,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return, // window closed; Drop stops motors
                };
                // What the port's output is set to, for the motor/servo sound.
                let value = match cmd {
                    Cmd::Drive(p, v) | Cmd::Set(p, Output::Motor | Output::Servo, v)
                        if !estop && linked[p as usize].is_none() =>
                    {
                        Some((p, v))
                    }
                    _ => None,
                };
                let stopping = matches!(cmd, Cmd::StopAll | Cmd::EStop(true));
                if stopping {
                    // "Stop all" also ends the tilt link; after an emergency
                    // stop the link re-applies once re-armed.
                    if matches!(cmd, Cmd::StopAll) {
                        tilt_link.store(false, Ordering::Relaxed);
                    }
                    linked = [None, None];
                    set_linked(&shared, &ctx, linked);
                }
                let r = match cmd {
                    Cmd::EStop(true) => {
                        estop = true;
                        hub.stop()
                    }
                    Cmd::EStop(false) => {
                        estop = false;
                        Ok(())
                    }
                    Cmd::Set(..) | Cmd::Drive(..) if estop => Ok(()),
                    // The tilt sensor owns this port.
                    Cmd::Set(p, ..) | Cmd::Drive(p, _) if linked[p as usize].is_some() => Ok(()),
                    Cmd::Drive(p, v) => match kinds[p as usize] {
                        Some(Kind::Servo) => hub.set_servo(p, v),
                        Some(Kind::LightBrick) => hub.set_light(p, v.abs()),
                        _ => hub.set_motor(p, v),
                    },
                    Cmd::Set(p, Output::Motor, v) => hub.set_motor(p, v),
                    Cmd::Set(p, Output::Servo, v) => hub.set_servo(p, v),
                    Cmd::Set(p, Output::Light, v) => hub.set_light(p, v),
                    Cmd::StopAll => hub.stop(),
                };
                if r.is_err() {
                    break 'connected;
                }
                if let Some((p, v)) = value {
                    out.set_value(p, v);
                }
                if stopping {
                    out.stop_all();
                }
            }
            match hub.read(Duration::from_millis(50)) {
                Ok(Some(reading)) => {
                    kinds = [Some(reading.a.kind()), Some(reading.b.kind())];
                    for (i, k) in [reading.a.kind(), reading.b.kind()].into_iter().enumerate() {
                        devices[i].update(k);
                        out.set_voice(
                            [Port::A, Port::B][i],
                            match devices[i].kind {
                                Some(Kind::Motor) => Voice::Motor,
                                Some(Kind::Servo) => Voice::Servo,
                                _ => Voice::None,
                            },
                        );
                    }
                    for (tr, a) in tilts.iter_mut().zip([reading.a, reading.b]) {
                        let t = if let Attached::Tilt(t) = a { Some(t) } else { None };
                        if let Some(t) = tr.update(t) {
                            if tilt_sound.load(Ordering::Relaxed) {
                                let _ = sound.tilt.send(t);
                            }
                        }
                    }
                    set(&shared, &ctx, true, Some(reading));

                    // Tilt sensor on one port drives the motor or servo on the other.
                    let want = if tilt_link.load(Ordering::Relaxed) && !estop {
                        tilt_drive([tilts[0].settled, tilts[1].settled], [devices[0].kind, devices[1].kind])
                    } else {
                        [None, None]
                    };
                    if want != linked && !estop {
                        for (i, p) in [Port::A, Port::B].into_iter().enumerate() {
                            if want[i] == linked[i] {
                                continue;
                            }
                            // Link ended: stop the port.
                            let v = want[i].unwrap_or(0);
                            let r = match devices[i].kind {
                                Some(Kind::Servo) => hub.set_servo(p, v),
                                _ => hub.set_motor(p, v),
                            };
                            if r.is_err() {
                                break 'connected;
                            }
                            out.set_value(p, v);
                            eprintln!("tilt link: port {p:?} = {v}{}", if want[i].is_none() { " (released)" } else { "" });
                        }
                        linked = want;
                        set_linked(&shared, &ctx, linked);
                    }
                }
                Ok(None) => {}
                Err(_) => break 'connected,
            }
        }
        out.stop_all();
        for p in [Port::A, Port::B] {
            out.set_voice(p, Voice::None);
        }
        set_linked(&shared, &ctx, [None, None]);
        set(&shared, &ctx, false, None);
    }
}

/// Reads the joystick and turns it into drive commands. Reconnects every second.
fn joystick_worker(joy: Arc<Mutex<JoyShared>>, tx: Sender<Cmd>, ctx: egui::Context) {
    loop {
        let Ok(mut js) = Joystick::open() else {
            thread::sleep(Duration::from_secs(1));
            continue;
        };
        {
            let mut j = joy.lock().unwrap();
            j.connected = true;
            j.name = js.name.clone();
            j.state = None;
            j.axis_names = js.axis_names();
            j.button_names = js.button_names();
            j.config = JoyConfig::load(&j.name, &j.axis_names, &j.button_names);
            j.learn = None;
        }
        ctx.request_repaint();
        let mut last = [0i32; 2];
        // After assigning the stop button, ignore it until it is released.
        let mut ignore_button = false;

        loop {
            {
                let mut j = joy.lock().unwrap();
                if std::mem::take(&mut j.recalibrate) {
                    js.recalibrate();
                }
                js.deadzone = j.config.deadzone;
            }
            let st = match js.read(Duration::from_millis(50)) {
                Ok(Some(st)) => st,
                Ok(None) => continue,
                Err(_) => break,
            };
            let mut j = joy.lock().unwrap();
            let mut repaint = false;
            if let (Some(learn), false) = (j.learn, st.centering) {
                match learn {
                    Learn::Axis(port) => {
                        let strongest = st.axes.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
                        if let Some((i, &v)) = strongest.filter(|(_, v)| v.abs() > 0.6) {
                            // The direction pushed is "forward" for this port.
                            j.config.axis[port] = Some(i);
                            j.config.invert[port] = v < 0.0;
                            j.learn = None;
                            j.save();
                            repaint = true;
                        }
                    }
                    Learn::EStop => {
                        if let Some(i) = st.buttons.iter().position(|&b| b) {
                            j.config.estop = Some(i);
                            j.learn = None;
                            ignore_button = true;
                            j.save();
                            repaint = true;
                        }
                    }
                }
            }
            let cfg = j.config.clone();
            let stick = [0, 1].map(|k| {
                let v = cfg.axis[k].and_then(|i| st.axes.get(i)).copied().unwrap_or(0.0);
                if cfg.invert[k] { -v } else { v }
            });
            let mut pressed = cfg.estop.and_then(|i| st.buttons.get(i)).copied().unwrap_or(false);
            ignore_button &= pressed;
            pressed &= !ignore_button;
            // Nothing drives while waiting to learn a control.
            let v = if j.learn.is_some() {
                [0, 0]
            } else {
                [0, 1].map(|k| (stick[k] * cfg.max[k] as f32).round() as i32)
            };
            if pressed && !j.estop {
                j.estop = true;
                let _ = tx.send(Cmd::EStop(true));
                last = [0, 0];
                j.drive = [0, 0];
                j.generation += 1;
            } else if j.estop && !pressed && v == [0, 0] && !st.centering {
                j.estop = false;
                let _ = tx.send(Cmd::EStop(false));
            }
            if !j.estop && !st.centering {
                for (i, port) in [Port::A, Port::B].into_iter().enumerate() {
                    if v[i] != last[i] {
                        let _ = tx.send(Cmd::Drive(port, v[i]));
                        last[i] = v[i];
                        j.drive[i] = v[i];
                        j.generation += 1;
                    }
                }
            }
            repaint |= j.state.as_ref().is_none_or(|old| {
                old.raw != st.raw || old.buttons != st.buttons || old.cal != st.cal || old.centering != st.centering
            });
            j.stick = stick;
            j.estop_pressed = pressed;
            j.state = Some(st);
            drop(j);
            if repaint {
                ctx.request_repaint();
            }
        }

        // Unplugged: don't leave motors running on the last stick position.
        for port in [Port::A, Port::B] {
            let _ = tx.send(Cmd::Drive(port, 0));
        }
        let mut j = joy.lock().unwrap();
        j.connected = false;
        j.state = None;
        j.learn = None;
        j.stick = [0.0; 2];
        j.estop_pressed = false;
        j.drive = [0, 0];
        j.generation += 1;
        drop(j);
        ctx.request_repaint();
    }
}

/// What the tilt sensor asks of the other port: a motor runs forward or back,
/// a servo steers left or right; level stops / centres it.
fn tilt_drive(tilt: [Option<Tilt>; 2], kinds: [Option<Kind>; 2]) -> [Option<i32>; 2] {
    let mut want = [None, None];
    for t_port in 0..2 {
        let (Some(t), out_port) = (tilt[t_port], 1 - t_port) else { continue };
        want[out_port] = match (kinds[out_port], t) {
            (Some(Kind::Motor), Tilt::Forward) => Some(TILT_SPEED),
            (Some(Kind::Motor), Tilt::Back) => Some(-TILT_SPEED),
            (Some(Kind::Motor), _) => Some(0),
            (Some(Kind::Servo), Tilt::Left) => Some(-100),
            (Some(Kind::Servo), Tilt::Right) => Some(100),
            (Some(Kind::Servo), _) => Some(0),
            _ => None,
        };
    }
    want
}

/// Debounces a port's device type. A short reading is ignored (a starting
/// motor pulls the ID low) and a motor's ID can flicker to "empty".
#[derive(Clone, Copy, Default)]
struct KindTracker {
    kind: Option<Kind>,
    candidate: Option<Kind>,
    count: u32,
}

impl KindTracker {
    fn update(&mut self, k: Kind) {
        if k == Kind::Short {
            return;
        }
        if self.kind == Some(k) {
            self.count = 0;
            return;
        }
        if self.candidate == Some(k) {
            self.count += 1;
        } else {
            self.candidate = Some(k);
            self.count = 1;
        }
        if self.kind.is_none() || self.count >= KIND_STABLE {
            self.kind = Some(k);
            self.count = 0;
        }
    }
}

fn set_linked(shared: &Mutex<Shared>, ctx: &egui::Context, linked: [Option<i32>; 2]) {
    let mut s = shared.lock().unwrap();
    if s.linked != linked {
        s.linked = linked;
        ctx.request_repaint();
    }
}

/// Debounces one port's tilt readings and reports settled direction changes.
/// The first direction after a sensor is plugged in is not reported.
#[derive(Clone, Copy, Default)]
struct TiltTracker {
    settled: Option<Tilt>,
    candidate: Option<Tilt>,
    count: u32,
}

impl TiltTracker {
    fn update(&mut self, t: Option<Tilt>) -> Option<Tilt> {
        let Some(t) = t else {
            *self = Self::default();
            return None;
        };
        if self.candidate == Some(t) {
            self.count += 1;
        } else {
            self.candidate = Some(t);
            self.count = 1;
        }
        if self.count < TILT_STABLE || self.settled == Some(t) {
            return None;
        }
        let first = self.settled.is_none();
        self.settled = Some(t);
        (!first).then_some(t)
    }
}

fn set(shared: &Mutex<Shared>, ctx: &egui::Context, connected: bool, reading: Option<Reading>) {
    let mut s = shared.lock().unwrap();
    let changed = s.connected != connected || format!("{:?}", s.reading) != format!("{reading:?}");
    s.connected = connected;
    if reading.is_some() || !connected {
        s.reading = reading;
    }
    if changed {
        ctx.request_repaint();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (connected, reading, linked) = {
            let s = self.shared.lock().unwrap();
            (s.connected, s.reading, s.linked)
        };
        for (i, l) in linked.iter().enumerate() {
            if let Some(v) = l {
                self.speed[i] = *v;
            }
        }
        if !connected {
            self.speed = [0, 0];
        }
        let (estop, rearm_ok) = {
            let j = self.joy.lock().unwrap();
            if j.generation != self.joy_generation {
                self.joy_generation = j.generation;
                self.speed = j.drive;
            }
            (j.estop, j.stick == [0.0; 2] && !j.estop_pressed)
        };
        let controls = connected && !estop;

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("WeDo Controller").size(22.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    status_badge(ui, connected);
                });
            });
            ui.add_space(8.0);
            if estop {
                egui::Frame::new()
                    .fill(RED)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("⛔  EMERGENCY STOP: all outputs off")
                                    .size(16.0)
                                    .strong()
                                    .color(Color32::WHITE),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add_enabled(rearm_ok, egui::Button::new("Re-arm")).clicked() {
                                    self.joy.lock().unwrap().estop = false;
                                    let _ = self.tx.send(Cmd::EStop(false));
                                }
                                ui.label(
                                    RichText::new("release the button and centre the stick")
                                        .color(Color32::WHITE),
                                );
                            });
                        });
                    });
                ui.add_space(8.0);
            }
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let stop = egui::Button::new(
                    RichText::new("⏹  Stop all")
                        .size(16.0)
                        .color(Color32::WHITE),
                )
                .fill(RED)
                .min_size(Vec2::new(120.0, 32.0));
                if ui.add_enabled(controls, stop).clicked() {
                    self.speed = [0, 0];
                    let _ = self.tx.send(Cmd::StopAll);
                }
                let mut on = self.tilt_sound.load(Ordering::Relaxed);
                if ui.checkbox(&mut on, "🔊 Tilt sound").changed() {
                    self.tilt_sound.store(on, Ordering::Relaxed);
                }
                let mut on = self.tilt_link.load(Ordering::Relaxed);
                if ui
                    .checkbox(&mut on, "🔗 Tilt drives motor")
                    .on_hover_text("Tilt sensor on one port: forward/back runs the motor on the other port, left/right steers a servo.")
                    .changed()
                {
                    self.tilt_link.store(on, Ordering::Relaxed);
                }
                let mut on = self.outputs.enabled.load(Ordering::Relaxed);
                if ui.checkbox(&mut on, "🔊 Motor sound").changed() {
                    self.outputs.enabled.store(on, Ordering::Relaxed);
                }
                if !connected {
                    ui.label(
                        RichText::new("Plug in the WeDo hub; it connects automatically.").weak(),
                    );
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(6.0);
            ui.columns(2, |cols| {
                for (i, (port, name)) in [(Port::A, "A"), (Port::B, "B")].into_iter().enumerate() {
                    let attached = reading.map(|r| if i == 0 { r.a } else { r.b });
                    let raw = reading.map(|r| r.raw[i]);
                    // Switch the control when a different output device is plugged in.
                    let mode = Output::for_kind(attached.map(Attached::kind));
                    if mode != self.mode[i] {
                        self.mode[i] = mode;
                        self.speed[i] = 0;
                        let _ = self.tx.send(Cmd::Set(port, mode, 0));
                    }
                    let tx = &self.tx;
                    port_card(
                        &mut cols[i],
                        name,
                        attached,
                        raw,
                        connected,
                        controls && linked[i].is_none(),
                        linked[i].is_some(),
                        mode,
                        &mut self.speed[i],
                        |v| {
                            let _ = tx.send(Cmd::Set(port, mode, v));
                        },
                    );
                }
            });
            ui.add_space(10.0);
            let mut j = self.joy.lock().unwrap();
            joystick_panel(ui, &mut j, &mut self.joy_config_open);
            joystick_config_window(ui.ctx(), &mut j, &mut self.joy_config_open);
        });
    }
}

/// Joystick status, live position and calibration of the mapped axes.
fn joystick_panel(ui: &mut egui::Ui, j: &mut JoyShared, config_open: &mut bool) {
    let cfg = j.config.clone();
    egui::Frame::group(ui.style())
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Joystick").size(18.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Configure").clicked() {
                        *config_open = !*config_open;
                    }
                    let (text, color) = if j.connected {
                        (format!("✔  {}", j.name), GREEN)
                    } else {
                        ("✘  Not connected".to_string(), RED)
                    };
                    ui.label(RichText::new(text).color(color).strong());
                });
            });
            if j.connected {
                ui.label(
                    RichText::new(format!(
                        "{} → Port A   ·   {} → Port B   ·   {} = emergency stop",
                        j.axis_name(cfg.axis[0]),
                        j.axis_name(cfg.axis[1]),
                        j.button_name(cfg.estop)
                    ))
                    .weak(),
                );
            }
            ui.separator();
            ui.horizontal(|ui| {
                // 2D pad: port A across, port B up (forward).
                let size = Vec2::splat(120.0);
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                let p = ui.painter_at(rect);
                let stroke = Stroke::new(1.0, ui.visuals().weak_text_color());
                p.rect_stroke(rect, CornerRadius::same(8), stroke, egui::StrokeKind::Inside);
                p.line_segment([rect.center_top(), rect.center_bottom()], stroke);
                p.line_segment([rect.left_center(), rect.right_center()], stroke);
                let dz = rect.width() / 2.0 * cfg.deadzone;
                p.rect_stroke(Rect::from_center_size(rect.center(), Vec2::splat(2.0 * dz)), 0, stroke, egui::StrokeKind::Middle);
                if j.state.is_some() {
                    let pos = Pos2::new(
                        rect.center().x + j.stick[0] * (rect.width() / 2.0 - 8.0),
                        rect.center().y - j.stick[1] * (rect.height() / 2.0 - 8.0),
                    );
                    p.circle_filled(pos, 7.0, if j.estop_pressed { RED } else { ACCENT });
                }

                ui.add_space(12.0);
                ui.vertical(|ui| {
                    match &j.state {
                        None if j.connected => {
                            ui.label("waiting for data…");
                        }
                        None => {
                            ui.label(RichText::new("Plug in a USB joystick or gamepad; it connects automatically.").weak());
                        }
                        Some(s) if s.centering => {
                            ui.label(RichText::new("Centring… leave the stick at rest.").color(ACCENT));
                        }
                        Some(s) => {
                            let mut all_ok = true;
                            for (k, port) in ["A", "B"].iter().enumerate() {
                                let Some(i) = cfg.axis[k] else { continue };
                                let (neg, pos) = s.cal.get(i).copied().unwrap_or_default();
                                all_ok &= neg && pos;
                                ui.horizontal(|ui| {
                                    ui.label(format!("Port {port}  {:+4.0} %", j.stick[k] * cfg.max[k] as f32));
                                    ui.label(RichText::new(format!("({})", j.axis_name(Some(i)))).weak());
                                    for (arrow, ok) in [("−", neg), ("+", pos)] {
                                        ui.label(arrow);
                                        ui.label(cal_mark(ok));
                                    }
                                });
                            }
                            if !all_ok {
                                ui.label(RichText::new("Push the stick fully in each direction once.").weak());
                            }
                            ui.label(if j.estop_pressed {
                                RichText::new("Stop button: pressed").color(RED).strong()
                            } else {
                                RichText::new("Stop button: released").weak()
                            });
                        }
                    }
                    ui.add_space(4.0);
                    if ui.add_enabled(j.connected, egui::Button::new("Recalibrate")).clicked() {
                        j.recalibrate = true;
                    }
                });
            });
        });
}

fn cal_mark(ok: bool) -> RichText {
    if ok {
        RichText::new("✔").color(GREEN)
    } else {
        RichText::new("○").weak()
    }
}

/// Mapping editor: axis per port with invert and top speed, stop button,
/// dead zone, plus a live view of every axis and button. Saves on change.
fn joystick_config_window(ctx: &egui::Context, j: &mut JoyShared, open: &mut bool) {
    let mut still_open = *open;
    egui::Window::new("Joypad configuration")
        .open(&mut still_open)
        .resizable(false)
        .collapsible(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            if !j.connected {
                ui.label(RichText::new("Plug in a joypad to configure it.").weak());
                return;
            }
            ui.label(RichText::new(&j.name).strong());
            ui.add_space(4.0);
            let mut cfg = j.config.clone();
            let mut learn = j.learn;
            egui::Grid::new("joy-map").num_columns(5).spacing([10.0, 8.0]).show(ui, |ui| {
                for (k, port) in ["A", "B"].iter().enumerate() {
                    ui.label(format!("Port {port}"));
                    let current = j.axis_name(cfg.axis[k]).to_string();
                    egui::ComboBox::from_id_salt(("joy-axis", k)).selected_text(current).width(130.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut cfg.axis[k], None, "— none —");
                        for (i, n) in j.axis_names.iter().enumerate() {
                            ui.selectable_value(&mut cfg.axis[k], Some(i), n);
                        }
                    });
                    ui.checkbox(&mut cfg.invert[k], "Invert");
                    learn_button(ui, &mut learn, Learn::Axis(k));
                    ui.add(egui::Slider::new(&mut cfg.max[k], 10..=100).suffix(" % max"));
                    ui.end_row();
                }
                ui.label("Stop button");
                let current = j.button_name(cfg.estop).to_string();
                egui::ComboBox::from_id_salt("joy-estop").selected_text(current).width(130.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut cfg.estop, None, "— none —");
                    for (i, n) in j.button_names.iter().enumerate() {
                        ui.selectable_value(&mut cfg.estop, Some(i), n);
                    }
                });
                ui.label("");
                learn_button(ui, &mut learn, Learn::EStop);
                ui.label("");
                ui.end_row();
                ui.label("Dead zone");
                let mut dz = (cfg.deadzone * 100.0).round() as i32;
                if ui.add(egui::Slider::new(&mut dz, 0..=40).suffix(" %")).changed() {
                    cfg.deadzone = dz as f32 / 100.0;
                }
                ui.end_row();
            });

            match learn {
                Some(Learn::Axis(k)) => {
                    let what = ["A", "B"][k];
                    ui.label(
                        RichText::new(format!(
                            "Push the stick or trigger the way that should run port {what} forward (servo: right)…"
                        ))
                        .color(ACCENT),
                    );
                }
                Some(Learn::EStop) => {
                    ui.label(RichText::new("Press the button to use as emergency stop…").color(ACCENT));
                }
                None => {}
            }
            if j.learn != learn {
                j.learn = learn;
            }
            if cfg != j.config {
                j.config = cfg;
                j.save();
            }

            ui.separator();
            ui.label(RichText::new("Inputs").strong());
            if let Some(s) = j.state.clone() {
                egui::Grid::new("joy-axes").num_columns(3).spacing([10.0, 4.0]).show(ui, |ui| {
                    for (i, name) in j.axis_names.iter().enumerate() {
                        let used = j.config.axis.contains(&Some(i));
                        let text = RichText::new(name);
                        ui.label(if used { text.color(ACCENT) } else { text });
                        axis_bar(ui, s.axes.get(i).copied().unwrap_or(0.0));
                        let (neg, pos) = s.cal.get(i).copied().unwrap_or_default();
                        ui.horizontal(|ui| {
                            ui.label(cal_mark(neg));
                            ui.label(cal_mark(pos));
                            ui.label(RichText::new(format!("{}", s.raw.get(i).copied().unwrap_or(0))).small().weak());
                        });
                        ui.end_row();
                    }
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for (i, name) in j.button_names.iter().enumerate() {
                        let on = s.buttons.get(i).copied().unwrap_or(false);
                        let text = RichText::new(name).small();
                        let text = if on { text.color(Color32::BLACK).background_color(ACCENT) } else { text.weak() };
                        ui.label(text);
                    }
                });
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Reset to defaults").clicked() {
                    j.config = JoyConfig::default_for(j.axis_names.len(), j.button_names.len());
                    j.learn = None;
                    j.save();
                }
                if ui.button("Recalibrate").clicked() {
                    j.recalibrate = true;
                }
                if let Some(e) = &j.save_error {
                    ui.label(RichText::new(format!("Couldn't save: {e}")).color(RED));
                } else {
                    ui.label(RichText::new("Saved automatically for this pad.").weak());
                }
            });
        });
    if !still_open {
        j.learn = None;
    }
    *open = still_open;
}

/// "Detect" toggles waiting for input for `target`.
fn learn_button(ui: &mut egui::Ui, learn: &mut Option<Learn>, target: Learn) {
    let active = *learn == Some(target);
    let text = if active { "Cancel" } else { "Detect" };
    if ui.selectable_label(active, text).clicked() {
        *learn = if active { None } else { Some(target) };
    }
}

/// Horizontal -1..1 bar with a centre line.
fn axis_bar(ui: &mut egui::Ui, v: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(160.0, 12.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), ui.visuals().extreme_bg_color);
    let c = rect.center().x;
    let x = c + v.clamp(-1.0, 1.0) * rect.width() / 2.0;
    let fill = Rect::from_x_y_ranges(c.min(x)..=c.max(x), rect.y_range());
    p.rect_filled(fill, 0, ACCENT);
    p.line_segment([rect.center_top(), rect.center_bottom()], Stroke::new(1.0, ui.visuals().weak_text_color()));
}

fn status_badge(ui: &mut egui::Ui, connected: bool) {
    let (icon, text, color) = if connected {
        ("✔", "Connected", GREEN)
    } else {
        ("✘", "Not connected", RED)
    };
    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .stroke(Stroke::new(1.0, color))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("{icon}  {text}"))
                    .color(color)
                    .strong(),
            );
        });
}

#[allow(clippy::too_many_arguments)]
fn port_card(
    ui: &mut egui::Ui,
    name: &str,
    attached: Option<Attached>,
    raw: Option<(u8, u8)>,
    connected: bool,
    controls: bool,
    tilt_linked: bool,
    mode: Output,
    value: &mut i32,
    mut send: impl FnMut(i32),
) {
    egui::Frame::group(ui.style())
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height(230.0);
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Port {name}")).size(18.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let color = if attached == Some(Attached::Short) {
                        RED
                    } else {
                        ACCENT
                    };
                    ui.label(RichText::new(device_name(attached, connected)).color(color));
                });
            });
            ui.separator();

            match attached {
                Some(Attached::Tilt(t)) => tilt_view(ui, t),
                Some(Attached::Distance { raw, percent }) => {
                    ui.label("Proximity (100 % = close)");
                    ui.add(
                        egui::ProgressBar::new(percent as f32 / 100.0)
                            .text(format!("{percent} %   (raw {raw})"))
                            .fill(ACCENT),
                    );
                }
                Some(Attached::Sensor { value, .. }) => {
                    ui.label("Value");
                    ui.add(
                        egui::ProgressBar::new(value as f32 / 255.0)
                            .text(format!("{value} / 255"))
                            .fill(ACCENT),
                    );
                }
                Some(Attached::Short) => {
                    ui.label(
                        RichText::new("Check the cable: the port reads as shorted.").color(RED),
                    );
                }
                _ => {}
            }

            // Sensors can't be driven; only show the output control for outputs or an empty port.
            let drivable = attached.is_none_or(|a| a.kind().is_output() || a == Attached::Empty);
            ui.add_space(8.0);
            if drivable {
                let (label, range, presets): (_, _, [(&str, i32); 3]) = match mode {
                    Output::Motor => (
                        "Motor speed",
                        -100..=100,
                        [("◀ −100", -100), ("■ 0", 0), ("100 ▶", 100)],
                    ),
                    Output::Servo => (
                        "Servo position",
                        -100..=100,
                        [("◀ left", -100), ("● centre", 0), ("right ▶", 100)],
                    ),
                    Output::Light => (
                        "Brightness",
                        0..=100,
                        [("off", 0), ("half", 50), ("full", 100)],
                    ),
                };
                ui.horizontal(|ui| {
                    ui.label(label);
                    if tilt_linked {
                        ui.label(RichText::new("🔗 driven by the tilt sensor").color(ACCENT));
                    }
                });
                ui.add_enabled_ui(controls, |ui| {
                    ui.spacing_mut().slider_width = ui.available_width() - 60.0;
                    let resp = ui.add(egui::Slider::new(value, range).suffix("%"));
                    ui.horizontal(|ui| {
                        let mut clicked = false;
                        for (text, v) in presets {
                            if ui.button(text).clicked() {
                                *value = v;
                                clicked = true;
                            }
                        }
                        if resp.changed() || clicked {
                            send(*value);
                        }
                    });
                });
            }

            if let Some((id, v)) = raw {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.label(
                        RichText::new(format!("raw id {id}  ·  value {v}"))
                            .small()
                            .weak(),
                    );
                });
            }
        });
}

fn device_name(a: Option<Attached>, connected: bool) -> &'static str {
    match (connected, a) {
        (false, _) => "—",
        (true, None) => "waiting…",
        (true, Some(a)) => a.kind().name(),
    }
}

/// A 3×3 compass of arrows; the current tilt direction lights up.
fn tilt_view(ui: &mut egui::Ui, t: Tilt) {
    ui.label(format!("Tilt: {t:?}"));
    let cell = |ui: &mut egui::Ui, s: &str, on: bool| {
        let text = RichText::new(s).size(22.0);
        let text = if on {
            text.color(ACCENT).strong()
        } else {
            text.weak()
        };
        ui.add_sized([36.0, 30.0], egui::Label::new(text));
    };
    egui::Grid::new(ui.next_auto_id())
        .spacing([2.0, 2.0])
        .show(ui, |ui| {
            ui.label("");
            cell(ui, "▲", t == Tilt::Forward);
            ui.label("");
            ui.end_row();
            cell(ui, "◀", t == Tilt::Left);
            cell(ui, "●", t == Tilt::Level);
            cell(ui, "▶", t == Tilt::Right);
            ui.end_row();
            ui.label("");
            cell(ui, "▼", t == Tilt::Back);
            ui.label("");
            ui.end_row();
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilt_tracker_debounces() {
        let mut t = TiltTracker::default();
        let feed = |t: &mut TiltTracker, v: Tilt, n: u32| (0..n).filter_map(|_| t.update(Some(v))).collect::<Vec<_>>();
        assert!(feed(&mut t, Tilt::Level, 5).is_empty()); // initial state is silent
        assert!(feed(&mut t, Tilt::Forward, 2).is_empty()); // too short: a glitch
        assert!(feed(&mut t, Tilt::Level, 3).is_empty()); // back to the same state
        assert_eq!(feed(&mut t, Tilt::Forward, 5), vec![Tilt::Forward]); // once, not repeated
        assert_eq!(feed(&mut t, Tilt::Level, 3), vec![Tilt::Level]);
        assert_eq!(t.update(None), None); // unplugged: reset
        assert!(feed(&mut t, Tilt::Back, 5).is_empty());
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn tilt_drives_other_port() {
        let (m, sv, t) = (Some(Kind::Motor), Some(Kind::Servo), Some(Kind::Tilt));
        assert_eq!(tilt_drive([Some(Tilt::Forward), None], [t, m]), [None, Some(TILT_SPEED)]);
        assert_eq!(tilt_drive([None, Some(Tilt::Back)], [m, t]), [Some(-TILT_SPEED), None]);
        assert_eq!(tilt_drive([Some(Tilt::Left), None], [t, m]), [None, Some(0)]);
        assert_eq!(tilt_drive([Some(Tilt::Right), None], [t, sv]), [None, Some(100)]);
        assert_eq!(tilt_drive([Some(Tilt::Forward), None], [t, Some(Kind::Empty)]), [None, None]);
        assert_eq!(tilt_drive([None, None], [m, m]), [None, None]);
    }

    #[test]
    fn kind_tracker_ignores_flicker() {
        let mut k = KindTracker::default();
        k.update(Kind::Motor);
        assert_eq!(k.kind, Some(Kind::Motor));
        for _ in 0..3 {
            k.update(Kind::Empty);
            k.update(Kind::Short);
        }
        k.update(Kind::Motor);
        assert_eq!(k.kind, Some(Kind::Motor));
        for _ in 0..KIND_STABLE {
            k.update(Kind::Empty);
        }
        assert_eq!(k.kind, Some(Kind::Empty)); // really unplugged
    }
}

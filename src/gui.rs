//! Desktop frontend. A worker thread owns the hub, reconnects when it is
//! unplugged, and publishes the latest readings; the UI only sends commands.
//! A second thread reads a USB joystick: X drives port A, Y drives port B,
//! and its button is a latching emergency stop.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Stroke, Vec2};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use wedo::joystick::{JoyState, Joystick};
use wedo::{Attached, Kind, Port, Reading, Tilt, WeDo};

const GREEN: Color32 = Color32::from_rgb(46, 160, 67);
const RED: Color32 = Color32::from_rgb(207, 70, 60);
const ACCENT: Color32 = Color32::from_rgb(240, 180, 30);

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
    /// Emergency stop latched: all outputs ignored until re-armed.
    estop: bool,
    /// Last values the joystick sent, and a counter so the UI notices new ones.
    drive: [i32; 2],
    generation: u64,
    recalibrate: bool,
    invert: [bool; 2],
}

impl Default for JoyShared {
    fn default() -> Self {
        Self {
            connected: false,
            name: String::new(),
            state: None,
            estop: false,
            drive: [0, 0],
            generation: 0,
            recalibrate: false,
            // HID Y grows downwards; invert so pushing forward runs the motor forward.
            invert: [false, true],
        }
    }
}

#[derive(Default)]
struct Shared {
    connected: bool,
    reading: Option<Reading>,
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
    tx: Sender<Cmd>,
    speed: [i32; 2],
    mode: [Output; 2],
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        let (s, c) = (shared.clone(), ctx.clone());
        thread::spawn(move || worker(s, rx, c));
        let joy = Arc::new(Mutex::new(JoyShared::default()));
        let (j, t, c) = (joy.clone(), tx.clone(), ctx.clone());
        thread::spawn(move || joystick_worker(j, t, c));
        Self {
            shared,
            joy,
            joy_generation: 0,
            tx,
            speed: [0, 0],
            mode: [Output::Motor; 2],
        }
    }
}

/// Owns the hub. Retries the connection every second while unplugged.
fn worker(shared: Arc<Mutex<Shared>>, rx: Receiver<Cmd>, ctx: egui::Context) {
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

        'connected: loop {
            loop {
                let r = match rx.try_recv() {
                    Ok(Cmd::EStop(true)) => {
                        estop = true;
                        hub.stop()
                    }
                    Ok(Cmd::EStop(false)) => {
                        estop = false;
                        Ok(())
                    }
                    Ok(Cmd::Set(..) | Cmd::Drive(..)) if estop => Ok(()),
                    Ok(Cmd::Drive(p, v)) => match kinds[p as usize] {
                        Some(Kind::Servo) => hub.set_servo(p, v),
                        Some(Kind::LightBrick) => hub.set_light(p, v.abs()),
                        _ => hub.set_motor(p, v),
                    },
                    Ok(Cmd::Set(p, Output::Motor, v)) => hub.set_motor(p, v),
                    Ok(Cmd::Set(p, Output::Servo, v)) => hub.set_servo(p, v),
                    Ok(Cmd::Set(p, Output::Light, v)) => hub.set_light(p, v),
                    Ok(Cmd::StopAll) => hub.stop(),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return, // window closed; Drop stops motors
                };
                if r.is_err() {
                    break 'connected;
                }
            }
            match hub.read(Duration::from_millis(50)) {
                Ok(Some(reading)) => {
                    kinds = [Some(reading.a.kind()), Some(reading.b.kind())];
                    set(&shared, &ctx, true, Some(reading));
                }
                Ok(None) => {}
                Err(_) => break 'connected,
            }
        }
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
        }
        ctx.request_repaint();
        let mut last = [0i32; 2];

        loop {
            if std::mem::take(&mut joy.lock().unwrap().recalibrate) {
                js.recalibrate();
            }
            let st = match js.read(Duration::from_millis(50)) {
                Ok(Some(st)) => st,
                Ok(None) => continue,
                Err(_) => break,
            };
            let mut j = joy.lock().unwrap();
            let sign = |inv: bool| if inv { -1.0 } else { 1.0 };
            let v = [
                (st.x * sign(j.invert[0]) * 100.0).round() as i32,
                (st.y * sign(j.invert[1]) * 100.0).round() as i32,
            ];
            if st.button && !j.estop {
                j.estop = true;
                let _ = tx.send(Cmd::EStop(true));
                last = [0, 0];
                j.drive = [0, 0];
                j.generation += 1;
            } else if j.estop && !st.button && v == [0, 0] && !st.centering {
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
            let changed = j.state.is_none_or(|old| {
                old.raw != st.raw || old.button != st.button || old.cal_x != st.cal_x || old.cal_y != st.cal_y
            });
            j.state = Some(st);
            drop(j);
            if changed {
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
        j.drive = [0, 0];
        j.generation += 1;
        drop(j);
        ctx.request_repaint();
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
        let (connected, reading) = {
            let s = self.shared.lock().unwrap();
            (s.connected, s.reading)
        };
        if !connected {
            self.speed = [0, 0];
        }
        let (joy_connected, joy_name, joy_state, estop, invert) = {
            let j = self.joy.lock().unwrap();
            if j.generation != self.joy_generation {
                self.joy_generation = j.generation;
                self.speed = j.drive;
            }
            (j.connected, j.name.clone(), j.state, j.estop, j.invert)
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
                                let centred = joy_state.is_none_or(|s| s.x == 0.0 && s.y == 0.0 && !s.button);
                                if ui.add_enabled(centred, egui::Button::new("Re-arm")).clicked() {
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
                        controls,
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
            let mut inv = invert;
            let recal = joystick_panel(ui, joy_connected, &joy_name, joy_state, &mut inv);
            j.invert = inv;
            j.recalibrate |= recal;
        });
    }
}

/// Joystick status, live position, calibration progress and options.
/// Returns true when "Recalibrate" was clicked.
fn joystick_panel(
    ui: &mut egui::Ui,
    connected: bool,
    name: &str,
    st: Option<JoyState>,
    invert: &mut [bool; 2],
) -> bool {
    let mut recal = false;
    egui::Frame::group(ui.style())
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Joystick").size(18.0).strong());
                ui.label(RichText::new("X → Port A   ·   Y → Port B   ·   button = emergency stop").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = if connected {
                        (format!("✔  {name}"), GREEN)
                    } else {
                        ("✘  Not connected".to_string(), RED)
                    };
                    ui.label(RichText::new(text).color(color).strong());
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                // 2D pad with the stick position.
                let size = Vec2::splat(120.0);
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                let p = ui.painter_at(rect);
                let stroke = Stroke::new(1.0, ui.visuals().weak_text_color());
                p.rect_stroke(rect, CornerRadius::same(8), stroke, egui::StrokeKind::Inside);
                p.line_segment([rect.center_top(), rect.center_bottom()], stroke);
                p.line_segment([rect.left_center(), rect.right_center()], stroke);
                let dz = rect.width() * 0.04;
                p.rect_stroke(Rect::from_center_size(rect.center(), Vec2::splat(2.0 * dz)), 0, stroke, egui::StrokeKind::Middle);
                if let Some(s) = st {
                    let pos = Pos2::new(
                        rect.center().x + s.x * (rect.width() / 2.0 - 8.0),
                        rect.center().y + s.y * (rect.height() / 2.0 - 8.0),
                    );
                    let color = if s.button { RED } else { ACCENT };
                    p.circle_filled(pos, 7.0, color);
                }

                ui.add_space(12.0);
                ui.vertical(|ui| {
                    match st {
                        None if connected => {
                            ui.label("waiting for data…");
                        }
                        None => {
                            ui.label(RichText::new("Plug in a USB joystick; it connects automatically.").weak());
                        }
                        Some(s) if s.centering => {
                            ui.label(RichText::new("Centring… leave the stick at rest.").color(ACCENT));
                        }
                        Some(s) => {
                            let mark = |ok: bool| if ok { RichText::new("✔").color(GREEN) } else { RichText::new("○").weak() };
                            ui.horizontal(|ui| {
                                ui.label("Calibration:");
                                for (arrow, ok) in [("◀", s.cal_x.0), ("▶", s.cal_x.1), ("▲", s.cal_y.0), ("▼", s.cal_y.1)] {
                                    ui.label(arrow);
                                    ui.label(mark(ok));
                                    ui.add_space(4.0);
                                }
                            });
                            if !(s.cal_x.0 && s.cal_x.1 && s.cal_y.0 && s.cal_y.1) {
                                ui.label(RichText::new("Push the stick fully in each direction once.").weak());
                            }
                            ui.label(format!(
                                "X {:+4.0} %   Y {:+4.0} %   (raw {}, {})",
                                s.x * 100.0,
                                s.y * 100.0,
                                s.raw.0,
                                s.raw.1
                            ));
                            ui.label(if s.button {
                                RichText::new("Button: pressed").color(RED).strong()
                            } else {
                                RichText::new("Button: released").weak()
                            });
                        }
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut invert[0], "Invert X");
                        ui.checkbox(&mut invert[1], "Invert Y");
                        if ui.add_enabled(connected, egui::Button::new("Recalibrate")).clicked() {
                            recal = true;
                        }
                    });
                });
            });
        });
    recal
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
                ui.label(label);
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

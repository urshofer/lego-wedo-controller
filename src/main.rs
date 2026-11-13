// On Windows, double-clicking the .exe opens only the GUI window (no console).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};
use wedo::{Attached, Port, Tilt, WeDo};

mod gui;
mod joyconfig;
mod sound;

const USAGE: &str = "\
Usage:
  wedo                             open the graphical controller
  wedo watch                       print sensor readings (Ctrl-C to quit)
  wedo motor <a|b> <speed> [secs]  run a motor, speed -100..100 (default 2 s)
  wedo stop                        stop both motors
  wedo demo                        tilt sensor steers a motor, distance sets speed";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    #[cfg(windows)]
    if !args.is_empty() {
        attach_parent_console();
    }

    if args.is_empty() {
        return match gui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("GUI error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if !matches!(
        args.as_slice(),
        ["watch"] | ["stop"] | ["demo"] | ["motor", _, _, ..]
    ) {
        return usage();
    }

    let mut hub = match WeDo::open() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Couldn't open the WeDo hub: {e}");
            eprintln!("Is it plugged in? On Linux, see the udev rule in the README.");
            return ExitCode::FAILURE;
        }
    };

    let result = match args.as_slice() {
        ["watch"] => watch(&mut hub),
        ["motor", port, speed, rest @ ..] => {
            let port = match port.to_ascii_lowercase().as_str() {
                "a" => Port::A,
                "b" => Port::B,
                _ => return usage(),
            };
            let Ok(speed) = speed.parse::<i32>() else {
                return usage();
            };
            let secs = rest
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(2.0);
            run_motor(&mut hub, port, speed, secs)
        }
        ["stop"] => hub.stop(),
        ["demo"] => demo(&mut hub),
        _ => return usage(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::FAILURE
}

fn describe(a: Attached) -> String {
    match a {
        Attached::Tilt(t) => format!("tilt {t:?}"),
        Attached::Distance { raw, percent } => format!("motion {percent:3}% (raw {raw})"),
        Attached::Sensor { kind, value } => format!("{} {value}", kind.name().to_lowercase()),
        other => other.kind().name().to_lowercase(),
    }
}

fn watch(hub: &mut WeDo) -> Result<(), hidapi::HidError> {
    let mut last = String::new();
    loop {
        if let Some(r) = hub.read(Duration::from_millis(500))? {
            let line = format!("A: {:<32} B: {}", describe(r.a), describe(r.b));
            if line != last {
                println!("{line}");
                last = line;
            }
        }
    }
}

fn run_motor(hub: &mut WeDo, port: Port, speed: i32, secs: f64) -> Result<(), hidapi::HidError> {
    hub.set_motor(port, speed)?;
    sleep(Duration::from_secs_f64(secs.max(0.0)));
    hub.stop()
}

/// Whichever port has a motor gets driven; the other port's sensor controls it.
fn demo(hub: &mut WeDo) -> Result<(), hidapi::HidError> {
    println!("Demo running for 60 s. Tilt forward/back or wave a hand at the distance sensor.");
    let start = Instant::now();
    // The first reports can show empty ports, and a motor's ID flickers, so
    // keep the last port a motor was seen on instead of giving up.
    let mut motor_port: Option<Port> = None;
    let mut hinted = false;
    while start.elapsed() < Duration::from_secs(60) {
        let Some(r) = hub.read(Duration::from_millis(200))? else {
            continue;
        };
        match (r.a, r.b) {
            (Attached::Motor, _) => motor_port = Some(Port::A),
            (_, Attached::Motor) => motor_port = Some(Port::B),
            _ => {}
        }
        let Some(motor_port) = motor_port else {
            if !hinted && start.elapsed() > Duration::from_secs(2) {
                eprintln!("Waiting for a motor: plug one into a port and a sensor into the other.");
                hinted = true;
            }
            continue;
        };
        let sensor = if motor_port == Port::A { r.b } else { r.a };
        let speed = match sensor {
            Attached::Tilt(Tilt::Forward) => 80,
            Attached::Tilt(Tilt::Back) => -80,
            Attached::Tilt(_) => 0,
            Attached::Distance { percent, .. } => percent as i32,
            _ => 0,
        };
        hub.set_motor(motor_port, speed)?;
    }
    hub.stop()
}

/// GUI-subsystem programs have no console; borrow the one we were started
/// from so the CLI subcommands can still print.
#[cfg(windows)]
fn attach_parent_console() {
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

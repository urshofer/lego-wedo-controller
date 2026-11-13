// On Windows, double-clicking the .exe opens only the GUI window (no console).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};
use wedo::{Attached, Port, Tilt, WeDo};

mod gui;

const USAGE: &str = "\
Usage:
  wedo                             open the graphical controller
  wedo web [port]                  serve the browser (WebHID) controller, default port 8080
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
    match args.as_slice() {
        ["web"] => return serve_web(8080),
        ["web", port] => return port.parse().map_or_else(|_| usage(), serve_web),
        _ => {}
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
    while start.elapsed() < Duration::from_secs(60) {
        let Some(r) = hub.read(Duration::from_millis(200))? else {
            continue;
        };
        let (motor_port, sensor) = match (r.a, r.b) {
            (Attached::Motor, s) => (Port::A, s),
            (s, Attached::Motor) => (Port::B, s),
            _ => {
                eprintln!("Plug a motor into one port and a sensor into the other.");
                return Ok(());
            }
        };
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

const WEB_PAGE: &str = include_str!("../web/index.html");

/// Serve the WebHID page. WebHID needs a secure context; http://localhost counts.
fn serve_web(port: u16) -> ExitCode {
    use std::io::{Read, Write};
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Couldn't listen on port {port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let url = format!("http://localhost:{port}/");
    println!("WeDo web controller at {url}  (Chrome/Edge/Opera; Ctrl-C to quit)");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();

    for stream in listener.incoming().flatten() {
        let mut stream = stream;
        let mut buf = [0u8; 2048];
        let n = stream.read(&mut buf).unwrap_or(0);
        let path = std::str::from_utf8(&buf[..n])
            .ok()
            .and_then(|r| r.split_whitespace().nth(1))
            .unwrap_or("/");
        let (status, ctype, body) = match path {
            "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", WEB_PAGE),
            _ => ("404 Not Found", "text/plain", "not found"),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    ExitCode::SUCCESS
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

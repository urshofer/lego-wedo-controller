//! Joypad mapping: which axis drives which port, which button is the
//! emergency stop. Saved per pad (by product name) in a small text file,
//! referring to axes and buttons by name so it survives layout changes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use wedo::joystick::DEFAULT_DEADZONE;

#[derive(Debug, Clone, PartialEq)]
pub struct JoyConfig {
    /// Axis index driving port A / port B, or none.
    pub axis: [Option<usize>; 2],
    /// Flip the direction per port.
    pub invert: [bool; 2],
    /// Top speed per port, 10..100 %.
    pub max: [i32; 2],
    pub estop: Option<usize>,
    pub deadzone: f32,
}

impl JoyConfig {
    /// First two axes, first button. HID Y grows downwards, so port B is
    /// inverted: pushing forward runs the motor forward.
    pub fn default_for(axes: usize, buttons: usize) -> Self {
        Self {
            axis: [(axes > 0).then_some(0), (axes > 1).then_some(1)],
            invert: [false, true],
            max: [100, 100],
            estop: (buttons > 0).then_some(0),
            deadzone: DEFAULT_DEADZONE,
        }
    }

    /// The saved mapping for this pad, or the default.
    pub fn load(pad: &str, axes: &[String], buttons: &[String]) -> Self {
        let mut c = Self::default_for(axes.len(), buttons.len());
        let Some(section) = read_file().remove(pad) else { return c };
        let find = |names: &[String], key: &str| -> Option<Option<usize>> {
            let v = section.get(key)?;
            if v.is_empty() {
                return Some(None);
            }
            names.iter().position(|n| n == v).map(Some)
        };
        for (i, p) in ["a", "b"].iter().enumerate() {
            if let Some(a) = find(axes, &format!("axis_{p}")) {
                c.axis[i] = a;
            }
            if let Some(v) = section.get(&format!("invert_{p}")) {
                c.invert[i] = v == "true";
            }
            if let Some(v) = section.get(&format!("max_{p}")).and_then(|v| v.parse::<i32>().ok()) {
                c.max[i] = v.clamp(10, 100);
            }
        }
        if let Some(b) = find(buttons, "estop") {
            c.estop = b;
        }
        if let Some(v) = section.get("deadzone").and_then(|v| v.parse::<f32>().ok()) {
            c.deadzone = v.clamp(0.0, 0.5);
        }
        c
    }

    pub fn save(&self, pad: &str, axes: &[String], buttons: &[String]) -> std::io::Result<()> {
        let name = |names: &[String], i: Option<usize>| i.and_then(|i| names.get(i)).cloned().unwrap_or_default();
        let mut s = BTreeMap::new();
        for (i, p) in ["a", "b"].iter().enumerate() {
            s.insert(format!("axis_{p}"), name(axes, self.axis[i]));
            s.insert(format!("invert_{p}"), self.invert[i].to_string());
            s.insert(format!("max_{p}"), self.max[i].to_string());
        }
        s.insert("estop".into(), name(buttons, self.estop));
        s.insert("deadzone".into(), format!("{:.2}", self.deadzone));
        let mut all = read_file();
        all.insert(pad.to_string(), s);
        let path = path().ok_or_else(|| std::io::Error::other("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, format_file(&all))
    }
}

type Sections = BTreeMap<String, BTreeMap<String, String>>;

fn path() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?
    };
    Some(base.join("wedo").join("joypad.conf"))
}

fn read_file() -> Sections {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|s| parse_file(&s)).unwrap_or_default()
}

/// `[pad name]` sections of `key = value` lines.
fn parse_file(s: &str) -> Sections {
    let mut all = Sections::new();
    let mut cur: Option<String> = None;
    for line in s.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            cur = Some(name.to_string());
            all.entry(name.to_string()).or_default();
        } else if let (Some(sec), Some((k, v))) = (&cur, line.split_once('=')) {
            all.get_mut(sec).unwrap().insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    all
}

fn format_file(all: &Sections) -> String {
    let mut out = String::from("# WeDo Controller joypad mappings, one section per pad.\n");
    for (pad, kv) in all {
        out += &format!("\n[{pad}]\n");
        for (k, v) in kv {
            out += &format!("{k} = {v}\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_round_trip() {
        let mut all = Sections::new();
        all.entry("Gamepad F310".into()).or_default().insert("axis_a".into(), "Right stick X".into());
        all.entry("Gamepad F310".into()).or_default().insert("estop".into(), "".into());
        assert_eq!(parse_file(&format_file(&all)), all);
    }
}

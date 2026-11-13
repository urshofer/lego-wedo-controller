# wedo

A small Rust library and command-line tool for the LEGO WeDo 1.0 USB hub
(9581 / 45303), using the `hidapi` crate. No drivers are needed on macOS or
Windows: the hub is a standard USB HID device.

## Build and run

    cargo build --release
    ./target/release/wedo              # desktop window (starts without the hub, auto-connects)
    ./target/release/wedo watch
    ./target/release/wedo motor a 60 3
    ./target/release/wedo demo

## Joystick

Any USB HID joystick or gamepad is picked up automatically (axes and buttons
are found by parsing its HID report descriptor). XInput pads (Xbox 360 style,
e.g. a Logitech F310/F710 with the switch on the back set to "X") are not HID
devices; they are read directly over USB via libusb. With the switch on "D" the
Logitech pads are plain HID gamepads.

By default the first axis drives port A, the second drives port B (pushing
forward = positive) and the first button is the emergency stop. A servo or
light brick on the port is driven as such.

- **⚙ Configure** opens the mapping: choose the axis for each port, invert it,
  limit its top speed, pick the emergency-stop button and set the dead zone.
  "Detect" assigns whatever you move or press next; for an axis, the direction
  you push becomes forward. A live view shows every axis and button.
- The mapping is saved per pad name in `joypad.conf` (macOS:
  `~/Library/Application Support/wedo/`, Windows: `%APPDATA%\wedo\`, Linux:
  `~/.config/wedo/`).
- The emergency stop latches: all outputs go to 0 and stay off until the button
  is released and the stick is back in the centre.
- Calibration is automatic: keep the stick at rest when it connects (centre),
  then push it fully in each direction once; the panel ticks off each direction.
  "Recalibrate" starts over.

## Tilt drives motor

With "🔗 Tilt drives motor" ticked (bottom of the window; on by default), a tilt sensor on one
port drives the other port, like `wedo demo`: tilting forward or back runs a
motor at ±100 %, level, left or right stops it; a servo steers left or right.
While linked, that port ignores the joystick and its slider. The emergency stop
still wins, and "Stop all" also switches the link off.

## Motor sound

Motors play a recording of a real crane that revs up and down with their speed
(slow to spin up and wind down; hoisting forward runs a little lower than
lowering); servos
play a gear whine while they travel (lower going left, higher going right) and
fall silent once in position. The "Motor sound" checkbox at the bottom turns
it off.

## Tilt sound

When a tilt sensor changes direction, a short tone plays: forward rises, back
falls, left is a low double beep, right a high one, and returning to level is a
soft tick. A direction has to be read three times in a row before it sounds, so
the sensor doesn't chatter at the edge between two positions. The "Tilt sound"
checkbox at the bottom turns it off.

## Linux

Building needs `libudev-dev` and `pkg-config`. To use the hub without sudo,
add `/etc/udev/rules.d/50-wedo.rules`:

    SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0694", ATTRS{idProduct}=="0003", MODE="0666"

then run `sudo udevadm control --reload && sudo udevadm trigger` and replug.

## Protocol

- Output report: `40 <motorA> <motorB> 00 00 00 00 00`, motor values as
  signed bytes (-127..127).
- Input report (8 bytes): byte 2 = port A value, byte 3 = port A device ID,
  byte 4 = port B value, byte 5 = port B device ID.
- Device IDs (from the ev3dev driver): short 0-9, bend 10-27, tilt 28-47,
  future 48-67, raw 68-87, touch 88-100, servo 101-109, sound 110-131,
  temperature 132-152, light 153-169, motion 170-190, light brick 191-211,
  reserved 212-224, empty 225-233, motor 234-246, short 247-255.
- Outputs: motor (speed), Power Functions servo (position), light brick
  (brightness) all use the same signed output byte.
- Motion sensor proximity: raw 71..219 maps to 0..100 %.

The retail WeDo 1.0 set only ships the motor, tilt and motion sensor; the other
IDs come from LEGO's resistor-ID scheme and are shown with their raw value.
Tilt thresholds may be mirrored on your sensor; `wedo watch` and the raw line
in the UI show the numbers.

## Credits

The motor sound (`assets/crane.raw`, a 15 s excerpt looped, mono 22 kHz) is
from ["WWS Coalcrane.ogg"](https://commons.wikimedia.org/wiki/File:WWS_Coalcrane.ogg),
a 1946 coal crane at the Bochum-Dahlhausen Railway Museum, recorded by Konrad
Gutkowski for [Work With Sounds](https://commons.wikimedia.org/wiki/Commons:Work_With_Sounds),
licensed [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). Cut,
crossfaded into a loop and level-normalised.

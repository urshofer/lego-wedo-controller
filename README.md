# wedo

A small Rust library and command-line tool for the LEGO WeDo 1.0 USB hub
(9581 / 45303), using the `hidapi` crate. No drivers are needed on macOS or
Windows: the hub is a standard USB HID device.

## Build and run

    cargo build --release
    ./target/release/wedo              # desktop window (starts without the hub, auto-connects)
    ./target/release/wedo web          # browser version at http://localhost:8080 (WebHID)
    ./target/release/wedo watch
    ./target/release/wedo motor a 60 3
    ./target/release/wedo demo

## Joystick

Any USB HID joystick or gamepad is picked up automatically (axes and button are
found by parsing its HID report descriptor). In the desktop app:

- X axis drives port A, Y axis drives port B (pushing forward = positive).
  A servo or light brick on the port is driven as such.
- Button 1 is a latching emergency stop: all outputs go to 0 and stay off
  until the button is released and the stick is back in the centre.
- Calibration is automatic: keep the stick at rest when it connects (centre),
  then push it fully in each direction once; the panel ticks off each direction.
  "Recalibrate" starts over, "Invert X/Y" flips a direction.

## Browser version

`wedo web [port]` serves `web/index.html` (compiled into the binary). It uses
WebHID, so it needs Chrome, Edge or Opera on desktop; Safari and Firefox lack
it. WebUSB can't be used because browsers block it for HID-class devices.
Only one program can hold the hub, so close the desktop window first.

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

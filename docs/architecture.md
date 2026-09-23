# Architecture

    +-----------+   frames   +----------------+   gamepad events   +-----------+
    |  Capture  | ---------> |     Agent      | -----------------> |  Gamepad  |
    | (emu fbuf |            | perceive/decide|                    |  (virtual |
    |  or HDMI) |            |     loop       |                    |  or USB)  |
    +-----------+            +----------------+                    +-----------+

Contracts:

- vision.BaseCapture: get_frame() returns a PIL Image (only input the agent ever sees)
- input.BaseGamepad: press(button), hold(button, frames), dpad(direction)
- emulator.EmulatorDriver: implements both, backed by headless emulator today.
  MUST NOT expose memory read/write, register peeks, or savestates to the agent.
  Battery saves (*.sav) allowed — same as a real cartridge.

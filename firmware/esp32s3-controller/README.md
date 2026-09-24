# ESP32-S3 WiFi Switch controller

Firmware that turns an ESP32-S3 into a **wired Nintendo Switch controller**:
a HORI Pokkén pad on the S3's native USB port, controlled **over WiFi**.

The firmware exposes the **full Switch layout**: all 18 buttons (Home,
Capture, ZL/ZR and the stick clicks included) and both analog sticks, as
`pokebot_core::SwitchCommand`. The bot's GBA `Controller` is an adapter on top
(`GbaOnSwitch`, Start → `+`, Select → `−`), so one firmware serves both.

```
bot (GBA ControllerCommand)
  └─ GbaOnSwitch ─┐
                  ├─ SwitchCommand ─ Esp32WifiController ──WiFi/TCP 7878──▶ ESP32-S3 ──USB HID──▶ Switch
menus, scripts ───┘                                                        (queue + 1 ms timing)
```

HTTP examples (`POST /api/command`; the body is a `SwitchCommand`):

```sh
curl -d '{"Press":"Home"}' http://<ip>/api/command
curl -d '{"Chord":["L","R"]}' http://<ip>/api/command     # register on Change Grip/Order
curl -d '{"Hold":{"state":{"buttons":["ZL"],"left_stick":{"x":128,"y":0}},"duration":{"secs":1,"nanos":0}}}' http://<ip>/api/command
curl -d '"Neutral"' http://<ip>/api/command
```

Sticks use x 0 = left … 255 = right and y 0 = up … 255 = down, with 128 as
centre. Omitted fields mean released or centred.

### Keepalive: the Switch never dims or sleeps

When the bot is stopped the Switch gets no input. After 5 minutes Screen
Burn-In Reduction dims the picture (which also changes the captured colours);
in TV mode Auto-Sleep then puts the console to sleep after 1, 2, 3, 6 or 12
hours (System Settings → Sleep Mode; "Never" is also an option). A sleeping
Switch stops the capture and cuts the controller's USB (`usb_mounted: false`),
and nobody may be there to wake it.

So after **4 minutes without input** the firmware nudges the **right stick**
up and down by itself (400 ms, no buttons). FireRed ignores the right stick,
so this is safe on every screen: the D-pad would walk the player (through a
door, into grass) and B would answer NO. Any command from a client cancels a
running nudge at once and restarts the idle clock; nudges are never reported
to clients as finished commands.

```sh
curl http://<ip>/api/keepalive                                   # settings and count
curl -d '{"after_secs": 120}' http://<ip>/api/keepalive          # idle time
curl -d '{"after_secs": null}' http://<ip>/api/keepalive         # off
curl -d '{"routine": {"Press": "B"}}' http://<ip>/api/keepalive  # other input
```

`GET /api/status` reports `keepalive_secs` and `keepalives` (routines played),
plus `usb_suspended`: the Switch suspended the bus (asleep).

### Waking a sleeping Switch

The configuration descriptor advertises **USB remote wakeup**. While the bus
is suspended, a report with a **button** pressed (not a stick, so never the
keepalive) makes the firmware request a wakeup, at most every 100 ms, like
HOME on a wired pad. The bot presses HOME every 30 s while the link is
`Suspended`. This works only if the Switch keeps the controller configured
while asleep and enables remote wakeup for it. If the dock powers its USB
ports down (`usb_mounted: false` while asleep), someone has to wake the
console.
Runtime changes last until the board restarts. Also set the Switch itself to
System Settings → Sleep Mode → Auto-Sleep (Playing on TV Screen) → Never, and
turn off Screen Burn-In Reduction if you want.

All protocol and timing logic is in [`crates/remote`](../../crates/remote),
which is shared with the host simulator and the emulator's virtual console.
This crate only brings up WiFi and TinyUSB. See
[`docs/architecture.md`](../../docs/architecture.md).

## Setup (once)

```sh
firmware/esp32s3-controller/tools/setup.sh
```

This installs apt packages, `espup` (Xtensa Rust toolchain `+esp`), `espflash`,
`ldproxy` and Espressif's QEMU, and adds you to `dialout`. The first firmware
build then downloads ESP-IDF v5.5.5 into `~/.espressif`, which takes about 10
minutes. Later builds are incremental.

## Development loops, fastest first

| Loop | Command | What runs |
|---|---|---|
| Unit + loopback tests | `cargo test -p pokebot-remote -p pokebot-esp32-wifi` | device code and bot client on the PC |
| Host simulator | `cargo run -p pokebot-remote --bin pokebot-remote-sim` | device code on the PC; prints HID reports; control page at http://127.0.0.1:8078/ |
| Emulator | `pokebot emulator serve`, then `pokebot run --video capture-card:/dev/video10 --controller esp32:127.0.0.1` | device code pressing the emulated game's buttons |
| QEMU | `tools/qemu.sh` | **the real Xtensa firmware** in Espressif QEMU; network forwarded to 127.0.0.1:7878 and :8078; HID reports logged (QEMU has no USB device) |
| Hardware | `tools/flash.sh` | the board |

Smoke test against any running device (simulator, QEMU or board):

```sh
POKEBOT_ESP32=127.0.0.1 cargo test -p pokebot-esp32-wifi --test hardware -- --nocapture
```

## Hardware bring-up

1. **Board ports.** Most ESP32-S3 DevKits have two USB-C ports:
   - **UART/COM** (USB–serial bridge): connect it to the PC to flash and read logs.
   - **USB/OTG** (native USB, GPIO19/20): this becomes the controller. Connect it to the Switch dock.

   With a single-port board, flash it first (hold BOOT, tap RESET), then move
   the cable to the Switch. Logs are then unavailable; use the HTTP status page.
2. **Flash.** Pass your WiFi credentials; they are baked in at build time:
   ```sh
   WIFI_SSID=myssid WIFI_PASS=secret firmware/esp32s3-controller/tools/flash.sh
   ```
   The monitor prints `control: <ip>:7878`. Without `WIFI_SSID`, the board
   starts its own access point `pokebot-controller` (password
   `pokebot-controller`, device at 192.168.71.1).
3. **Before the Switch:** plug the native port into the PC. `lsusb` should
   list `0f0d:0092 HORI CO.,LTD. POKKEN CONTROLLER`, and
   http://&lt;ip&gt;/ should report `usb mounted`. Buttons pressed on the page
   show up in `evtest` / `jstest-gtk`.
4. **Switch:** enable *System Settings → Controllers and Sensors → Pro
   Controller Wired Communication*, then dock and plug in. The wired pad may
   need a button press on *Change Grip/Order* to register (use the page's A
   or L+R).
5. **Bot:** `pokebot run --video capture-card:/dev/video0 --controller esp32:<ip>`.

## Layout

```
src/main.rs                 WiFi / OpenETH bring-up, TinyUSB sink, runs pokebot_remote::Device
components/switch_hid/      C glue: TinyUSB descriptors + HID callbacks (esp_tinyusb 1.7)
sdkconfig.defaults          1 kHz FreeRTOS tick, console on UART0, TinyUSB HID
sdkconfig.qemu              OpenCores Ethernet for QEMU
tools/                      setup.sh, build.sh, flash.sh, qemu.sh
```

## Notes and known gaps

- esp-idf-sys 0.38 generates bindings for esp_tinyusb's **1.x** header
  layout, so the component is pinned to `~1.7.6`.
- ESP-IDF sockets cannot be `dup`ed, so the server never calls
  `TcpStream::try_clone`. Keep that in mind when changing `crates/remote`:
  the host simulator will not catch it, but QEMU will.
- QEMU only boots with ADC calibration in eFuse. `tools/qemu.sh` writes a
  matching eFuse image.
- Protocol v2 carries `SwitchCommand`s; the client refuses v1 devices, so
  reflash after pulling.
- The PABotBase adapter still has its own GBA → Switch table
  (`adapters/pabotbase/src/report.rs`). It could implement `SwitchController`
  and share `GbaOnSwitch`.
- Not yet tested on a real Switch.

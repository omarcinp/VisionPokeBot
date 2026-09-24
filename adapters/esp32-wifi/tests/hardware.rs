//! Smoke test against a running device: the ESP32-S3, the firmware in QEMU,
//! or the simulator. Skipped unless `POKEBOT_ESP32` names it:
//!
//!     POKEBOT_ESP32=192.168.1.50 cargo test -p pokebot-esp32-wifi --test hardware -- --nocapture
//!
//! Every GBA button is tapped once, so run it with the Switch on a screen
//! where that is harmless (e.g. the "Change Grip/Order" screen).

use std::time::{Duration, Instant};

use pokebot_core::{Button, Controller, ControllerCommand, GbaOnSwitch};
use pokebot_esp32_wifi::{Esp32WifiConfig, Esp32WifiController};

#[test]
fn taps_every_button_and_completes() {
    let Ok(address) = std::env::var("POKEBOT_ESP32") else {
        eprintln!("POKEBOT_ESP32 not set; skipping");
        return;
    };
    let mut switch = Esp32WifiController::connect(Esp32WifiConfig::new(address)).unwrap();
    eprintln!("connected to {} {:?}", switch.peer(), switch.info());
    let status = switch.status().unwrap();
    eprintln!("status {status:?}");
    if !status.usb_mounted {
        eprintln!("warning: USB not mounted by the Switch; reports go nowhere");
    }
    // The bot's view: GBA buttons through the adapter. (Home and Capture are
    // not tapped, as they would leave the game.)
    let mut controller = GbaOnSwitch(switch);
    for button in Button::ALL {
        let issued = Instant::now();
        let receipt = controller
            .execute(ControllerCommand::Press(button))
            .unwrap();
        let accepted = issued.elapsed();
        while !controller.is_idle().unwrap() {
            assert!(
                issued.elapsed() < Duration::from_secs(2),
                "{button} never finished"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let finished = issued.elapsed();
        eprintln!(
            "{button:<6} accepted after {:>6.1} ms, finished after {:>6.1} ms (input {} ms)",
            accepted.as_secs_f64() * 1e3,
            finished.as_secs_f64() * 1e3,
            receipt.input_duration.as_millis()
        );
        assert!(finished >= receipt.input_duration);
    }
    assert!(controller.inner_mut().status().unwrap().idle);
}

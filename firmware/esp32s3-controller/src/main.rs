//! ESP32-S3 firmware: a USB wired Switch controller (HORI Pokkén pad) whose
//! buttons are driven over WiFi by the bot.
//!
//! It is a full Switch controller (every button, both sticks); the bot's GBA
//! commands are mapped onto it client-side by `pokebot_core::GbaOnSwitch`.
//! Everything protocol-related lives in `pokebot-remote` and is shared with
//! the host simulator; this file only brings up the network and USB.
//!
//! * Network: WiFi station if built with `WIFI_SSID`/`WIFI_PASS`, otherwise
//!   an access point `pokebot-controller` (password `pokebot-controller`).
//!   Under QEMU (`--features qemu`): the emulated OpenCores Ethernet.
//! * USB: TinyUSB on the native USB port (GPIO19/20). Under QEMU reports are
//!   logged instead.
//! * API: control protocol on TCP 7878, HTTP + control page on port 80.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use log::{info, warn};
use pokebot_remote::protocol::CONTROL_PORT;
use pokebot_remote::{Device, DeviceConfig};

const HTTP_PORT: u16 = 80;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    info!(
        "pokebot-esp32s3-controller {} starting",
        env!("CARGO_PKG_VERSION")
    );

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;

    let sink = usb::start()?;
    let mut network = net::start(peripherals, sysloop)?;

    let name = option_env!("DEVICE_NAME").unwrap_or("PokeBot ESP32-S3 controller");
    let device = Device::start(
        DeviceConfig::new(name, Ipv4Addr::UNSPECIFIED.into(), CONTROL_PORT, HTTP_PORT),
        sink,
    )?;
    let ip = network.ip()?;
    info!("control: {ip}:{}  (pokebot --controller esp32:{ip})", device.control_addr().port());
    info!("http:    http://{ip}:{}/", device.http_addr().port());

    let mut last_mounted = None;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let mounted = device.status().usb_mounted;
        if last_mounted != Some(mounted) {
            info!("usb: {}", if mounted { "mounted by host" } else { "not mounted" });
            last_mounted = Some(mounted);
        }
        if let Err(e) = network.keep_alive() {
            warn!("network: {e}");
        }
    }
}

#[cfg(not(feature = "qemu"))]
mod usb {
    use anyhow::{bail, Result};
    use esp_idf_svc::sys::{switch_hid, ESP_OK};
    use pokebot_remote::hid::{self, SwitchReport};
    use pokebot_remote::HidSink;

    struct Config(switch_hid::switch_hid_config_t);
    // SAFETY: only holds pointers to 'static, immutable data.
    unsafe impl Sync for Config {}

    /// Handed to C for the life of the program.
    static CONFIG: Config = Config(switch_hid::switch_hid_config_t {
        vendor_id: hid::VENDOR_ID,
        product_id: hid::PRODUCT_ID,
        bcd_device: 0x0100,
        manufacturer: hid::MANUFACTURER.as_ptr(),
        product: hid::PRODUCT.as_ptr(),
        serial: c"000000000001".as_ptr(),
        report_descriptor: hid::REPORT_DESCRIPTOR.as_ptr(),
        report_descriptor_len: hid::REPORT_DESCRIPTOR.len() as u16,
        poll_interval_ms: hid::POLL_INTERVAL_MS,
    });

    pub struct TinyUsbSink;

    impl HidSink for TinyUsbSink {
        fn send(&mut self, report: &SwitchReport) -> bool {
            let bytes = report.to_bytes();
            // SAFETY: valid buffer of the given length; TinyUSB copies it.
            unsafe { switch_hid::switch_hid_send(bytes.as_ptr(), bytes.len()) }
        }

        fn mounted(&self) -> bool {
            // SAFETY: plain query of TinyUSB state.
            unsafe { switch_hid::switch_hid_mounted() }
        }
    }

    pub fn start() -> Result<TinyUsbSink> {
        // SAFETY: CONFIG and everything it points to is 'static.
        let err = unsafe { switch_hid::switch_hid_init(&CONFIG.0) };
        if err != ESP_OK {
            bail!("TinyUSB install failed: {err}");
        }
        Ok(TinyUsbSink)
    }
}

#[cfg(feature = "qemu")]
mod usb {
    use anyhow::Result;
    use log::info;
    use pokebot_remote::{HidSink, SwitchReport};

    /// QEMU has no USB device controller: log what the Switch would receive.
    pub struct LogSink(Option<SwitchReport>);

    impl HidSink for LogSink {
        fn send(&mut self, report: &SwitchReport) -> bool {
            if self.0 != Some(*report) {
                let state = report.to_state();
                info!(
                    "hid {:<20} L({},{}) R({},{}) {:02x?}",
                    state.buttons.to_string(),
                    state.left_stick.x,
                    state.left_stick.y,
                    state.right_stick.x,
                    state.right_stick.y,
                    report.to_bytes()
                );
                self.0 = Some(*report);
            }
            true
        }

        fn mounted(&self) -> bool {
            true
        }
    }

    pub fn start() -> Result<LogSink> {
        info!("usb: QEMU build, HID reports are logged");
        Ok(LogSink(None))
    }
}

#[cfg(not(feature = "qemu"))]
mod net {
    use std::net::Ipv4Addr;

    use anyhow::{anyhow, Result};
    use esp_idf_svc::eventloop::EspSystemEventLoop;
    use esp_idf_svc::hal::peripherals::Peripherals;
    use esp_idf_svc::nvs::EspDefaultNvsPartition;
    use esp_idf_svc::sys::{esp, esp_wifi_set_ps, wifi_ps_type_t_WIFI_PS_NONE};
    use esp_idf_svc::wifi::{
        AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration,
        EspWifi,
    };
    use log::{info, warn};

    const AP_SSID: &str = "pokebot-controller";
    const AP_PASS: &str = "pokebot-controller";

    pub struct Network {
        wifi: BlockingWifi<EspWifi<'static>>,
        station: bool,
    }

    pub fn start(peripherals: Peripherals, sysloop: EspSystemEventLoop) -> Result<Network> {
        let nvs = EspDefaultNvsPartition::take()?;
        let mut wifi = BlockingWifi::wrap(
            EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
            sysloop,
        )?;
        let ssid = option_env!("WIFI_SSID").unwrap_or("");
        let station = !ssid.is_empty();
        let config = if station {
            let pass = option_env!("WIFI_PASS").unwrap_or("");
            Configuration::Client(ClientConfiguration {
                ssid: ssid.try_into().map_err(|_| anyhow!("WIFI_SSID too long"))?,
                password: pass.try_into().map_err(|_| anyhow!("WIFI_PASS too long"))?,
                auth_method: if pass.is_empty() {
                    AuthMethod::None
                } else {
                    AuthMethod::WPA2Personal
                },
                ..Default::default()
            })
        } else {
            Configuration::AccessPoint(AccessPointConfiguration {
                ssid: AP_SSID.try_into().unwrap(),
                password: AP_PASS.try_into().unwrap(),
                auth_method: AuthMethod::WPA2Personal,
                channel: 6,
                ..Default::default()
            })
        };
        wifi.set_configuration(&config)?;
        wifi.start()?;
        // Modem sleep adds up to ~100 ms of latency per packet.
        // SAFETY: WiFi is started.
        esp!(unsafe { esp_wifi_set_ps(wifi_ps_type_t_WIFI_PS_NONE) })?;
        let mut network = Network { wifi, station };
        if station {
            info!("wifi: joining {ssid:?}");
            while let Err(e) = network.connect() {
                warn!("wifi: {e}; retrying");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        } else {
            network.wifi.wait_netif_up()?;
            info!("wifi: no WIFI_SSID at build time; access point {AP_SSID:?} password {AP_PASS:?}");
        }
        Ok(network)
    }

    impl Network {
        fn connect(&mut self) -> Result<()> {
            self.wifi.connect()?;
            self.wifi.wait_netif_up()?;
            Ok(())
        }

        pub fn ip(&self) -> Result<Ipv4Addr> {
            let netif = if self.station {
                self.wifi.wifi().sta_netif()
            } else {
                self.wifi.wifi().ap_netif()
            };
            Ok(netif.get_ip_info()?.ip)
        }

        /// Rejoins the network after a drop-out.
        pub fn keep_alive(&mut self) -> Result<()> {
            if self.station && !self.wifi.is_connected()? {
                warn!("wifi: disconnected, reconnecting");
                self.connect()?;
                info!("wifi: back online at {}", self.ip()?);
            }
            Ok(())
        }
    }
}

#[cfg(feature = "qemu")]
mod net {
    use std::net::Ipv4Addr;

    use anyhow::Result;
    use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver, OpenEth};
    use esp_idf_svc::eventloop::EspSystemEventLoop;
    use esp_idf_svc::hal::peripherals::Peripherals;
    use log::info;

    pub struct Network {
        eth: BlockingEth<EspEth<'static, OpenEth>>,
    }

    pub fn start(peripherals: Peripherals, sysloop: EspSystemEventLoop) -> Result<Network> {
        let driver = EthDriver::new_openeth(peripherals.mac, sysloop.clone())?;
        let mut eth = BlockingEth::wrap(EspEth::wrap(driver)?, sysloop)?;
        eth.start()?;
        eth.wait_netif_up()?;
        info!("eth: QEMU OpenETH up");
        Ok(Network { eth })
    }

    impl Network {
        pub fn ip(&self) -> Result<Ipv4Addr> {
            Ok(self.eth.eth().netif().get_ip_info()?.ip)
        }

        pub fn keep_alive(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

fn main() {
    embuild::espidf::sysenv::output();
    for var in ["WIFI_SSID", "WIFI_PASS", "DEVICE_NAME"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}

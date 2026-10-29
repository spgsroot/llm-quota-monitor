fn main() {
    for variable in ["WIFI_SSID", "WIFI_PASSWORD", "COLLECTOR_URL"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    embuild::espidf::sysenv::output();
}

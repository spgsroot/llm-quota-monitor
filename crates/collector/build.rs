fn main() {
    println!("cargo:rerun-if-changed=assets/quota-monitor.ico");
    println!("cargo:rerun-if-changed=assets/quota-monitor.svg");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/quota-monitor.ico");
        resource
            .compile()
            .expect("Cannot embed the application icon in the Windows executable");
    }
}

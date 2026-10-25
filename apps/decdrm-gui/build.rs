//! Embeds the app icon (assets/icon/decdrm.ico) and version information in the Windows
//! executable, for Explorer and the taskbar. Other targets need nothing.

fn main() {
    const ICON: &str = "../../assets/icon/decdrm.ico";
    println!("cargo:rerun-if-changed={ICON}");
    // Build scripts run on the host, so the target is read from cargo's environment
    // rather than `cfg!(windows)`.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon(ICON)
            .compile_for("decdrm-gui")
            .expect("compiling the Windows resource (icon) failed");
    }
}

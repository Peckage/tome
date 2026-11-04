// Windows: embed the app icon and version information in the executable,
// so Explorer, the taskbar and the Start menu show Tome's icon.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=data/tome.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("data/tome.ico");
        res.set("ProductName", "Tome");
        res.set("FileDescription", "Tome");
        res.set("LegalCopyright", "MIT License");
        res.compile().expect("could not embed the Windows resources (needs rc.exe from the Windows SDK)");
    }
}

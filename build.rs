//! Embeds the application icon and version info into the Windows executables.

fn main() {
    println!("cargo:rerun-if-changed=assets/rust_audacity.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/rust_audacity.ico")
            .set("ProductName", "Rust Audacity")
            .set("FileDescription", "Rust Audacity – audio editor")
            .set("CompanyName", "Ready Rock Media");
        // Missing resource compiler (rc.exe / windres) should not stop the build.
        if let Err(e) = res.compile() {
            println!(
                "cargo:warning=could not embed the app icon ({e}); the program still works, only the .exe icon is missing"
            );
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=assets/branding/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Pull the numeric and string file/product versions from Cargo.toml.
        // Windows Explorer then reports the same version as the tagged release.
        winresource::WindowsResource::new()
            .set_icon("assets/branding/app.ico")
            .compile()
            .expect("could not embed the Windows version resource; install the Windows SDK");
    }
}

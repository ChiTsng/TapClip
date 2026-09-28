use std::path::Path;

fn main() {
    // Embed the application manifest (per-monitor DPI awareness, modern common
    // controls) through the MSVC linker, so no resource compiler is needed.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("tapclip.exe.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    }
}

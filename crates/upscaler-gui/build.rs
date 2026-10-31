//! Embeds the icon, manifest (per-monitor DPI) and version info in the Windows exe.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest_file("AIVideoUpscaler.exe.manifest")
            .set_icon("assets/icon.ico")
            .set("ProductName", "AI Video Upscaler")
            .set("FileDescription", "AI Video Upscaler")
            .set("InternalName", "AIVideoUpscaler")
            .set("OriginalFilename", "AIVideoUpscaler.exe");
        res.compile_for("AIVideoUpscaler")
            .expect("unable to compile Windows resources");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=AIVideoUpscaler.exe.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}

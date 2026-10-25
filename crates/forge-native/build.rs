//! On Windows, puts Forge's icon in forge.exe (Explorer, the taskbar and shortcuts show it).

fn main() {
    println!("cargo:rerun-if-changed=assets/images/app_icon.ico");
    println!("cargo:rerun-if-changed=forge.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("forge.rc", embed_resource::NONE).manifest_optional().expect("compiling forge.rc");
    }
}

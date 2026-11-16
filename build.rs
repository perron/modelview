fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/modelview.ico");
    #[cfg(target_os = "windows")]
    embed_windows_icon();
}

#[cfg(target_os = "windows")]
fn embed_windows_icon() {
    let ico = std::env::current_dir()
        .expect("crate directory")
        .join("packaging/windows/modelview.ico");
    assert!(ico.is_file(), "missing {}", ico.display());
    let rc_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("modelview.rc");
    // RC.EXE resolves the ICON path from the working directory. An absolute
    // path with forward slashes works for both RC.EXE and windres.
    let icon_path = ico.display().to_string().replace('\\', "/");
    std::fs::write(&rc_path, format!("1 ICON \"{icon_path}\"\n"))
        .expect("write icon resource script");
    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

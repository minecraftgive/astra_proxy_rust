mod metadata {
    include!("../astra-platform/build_metadata.rs");
    pub fn emit() {
        main();
    }
}
fn main() {
    metadata::emit();
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if !std::env::var("TARGET")
        .unwrap_or_default()
        .contains("windows")
    {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let icon = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"),
    )
    .join("assets/icon.ico");
    let resource = out.join("astra-icon.rc");
    let compiled = out.join("astra-icon.res");
    let quoted = icon
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    std::fs::write(&resource, format!("1 ICON \"{quoted}\"\n"))
        .expect("Write Windows icon resource");
    let status = std::process::Command::new("rc.exe")
        .arg("/nologo")
        .arg("/fo")
        .arg(&compiled)
        .arg(&resource)
        .status()
        .expect("Windows SDK rc.exe is required to embed the application icon");
    assert!(status.success(), "Compile Windows application icon");
    println!("cargo:rustc-link-arg-bins={}", compiled.display());
}

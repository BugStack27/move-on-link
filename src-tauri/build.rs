fn main() {
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let manifest = if profile == "release" {
        include_str!("app.manifest")
    } else {
        include_str!("app.dev.manifest")
    };
    let windows = tauri_build::WindowsAttributes::new().app_manifest(manifest);
    let attributes = tauri_build::Attributes::new().windows_attributes(windows);
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}

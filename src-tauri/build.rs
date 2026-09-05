use std::path::PathBuf;

fn main() {
    tauri_build::build();

    let is_windows_msvc =
        std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
            && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    if is_windows_msvc {
        let manifest = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR")
                .expect("CARGO_MANIFEST_DIR must be available"),
        )
        .join("tests")
        .join("windows-test.manifest");

        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}

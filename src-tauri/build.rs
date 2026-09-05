use std::path::PathBuf;

fn main() {
    tauri_build::build();

    println!("cargo:rerun-if-env-changed=REPOTUNNEL_WINDOWS_TEST_MANIFEST");

    let embed_test_manifest = std::env::var("REPOTUNNEL_WINDOWS_TEST_MANIFEST").as_deref()
        == Ok("1")
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    if embed_test_manifest {
        let manifest = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be available"),
        )
        .join("tests")
        .join("windows-test.manifest");

        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
}

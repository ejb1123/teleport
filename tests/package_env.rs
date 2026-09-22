#![cfg(target_os = "linux")]

/// Model an Arch shell overriding the Nix SDL runtime. The inert shared object
/// is a valid ELF library but deliberately not SDL3. No real device is modified.
#[test]
#[ignore = "requires the wrapped Nix package and a C compiler"]
fn packaged_key_listing_ignores_foreign_library_and_font_overrides() {
    let binary = std::env::var_os("TELEPORT_TEST_BINARY")
        .expect("set TELEPORT_TEST_BINARY to the wrapped Nix executable");
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("foreign.c");
    let library = temp.path().join("libSDL3.so.0");
    std::fs::write(
        &source,
        "int teleport_foreign_library_fixture(void) { return 0; }\n",
    )
    .unwrap();
    assert!(
        std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-o"])
            .arg(&library)
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );
    let output = std::process::Command::new(binary)
        .args(["security-key", "list"])
        .env("LD_LIBRARY_PATH", temp.path())
        .env("LD_PRELOAD", &library)
        .env("SDL_DYNAMIC_API", &library)
        .env("SDL3_DYNAMIC_API", &library)
        .env("FONTCONFIG_FILE", "/nonexistent/teleport-fontconfig.xml")
        .env("FONTCONFIG_PATH", "/nonexistent/teleport-fontconfig")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "packaged key listing failed: {errors}"
    );
    for diagnostic in [
        "Failed loading",
        "SDL_DYNAMIC_API",
        "Fontconfig warning",
        "Fontconfig error",
    ] {
        assert!(
            !errors.contains(diagnostic),
            "foreign configuration leaked: {errors}"
        );
    }
}

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

/// Key diagnostics intentionally skip GStreamer, so they do not exercise the
/// scanner's fontconfig initialization. A fresh media registry does, including
/// in shells whose host fontconfig rules require a newer XML parser.
#[test]
#[ignore = "requires the wrapped Nix package with media plugins"]
fn packaged_fresh_media_scan_does_not_import_host_fontconfig() {
    let binary = std::env::var_os("TELEPORT_TEST_BINARY")
        .expect("set TELEPORT_TEST_BINARY to the wrapped Nix executable");
    let temp = tempfile::tempdir().unwrap();
    let config_home = temp.path().join("config");
    let user_fonts = temp.path().join("foreign-fontconfig");
    std::fs::create_dir_all(&user_fonts).unwrap();
    std::fs::write(
        user_fonts.join("fonts.conf"),
        "<fontconfig><deliberately-invalid-teleport-test/></fontconfig>",
    )
    .unwrap();
    let output = std::process::Command::new(binary)
        .arg("doctor")
        .env("GST_REGISTRY", temp.path().join("fresh-registry.bin"))
        .env("GST_REGISTRY_1_0", temp.path().join("fresh-registry.bin"))
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("FONTCONFIG_FILE", user_fonts.join("fonts.conf"))
        .env("FONTCONFIG_PATH", &user_fonts)
        // Fontconfig's positive trace proves it ran and shows what it loaded.
        .env("FC_DEBUG", "1024")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&output.stderr);
    let trace = format!("{}\n{errors}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "packaged media scan failed: {trace}"
    );
    assert!(
        trace.contains("teleport-fonts.conf"),
        "fontconfig was not exercised with the package config: {trace}"
    );
    for diagnostic in [
        "Fontconfig warning",
        "Fontconfig error",
        "/etc/fonts/conf.d",
        "deliberately-invalid-teleport-test",
    ] {
        assert!(
            !trace.contains(diagnostic),
            "foreign fontconfig leaked into media initialization: {trace}"
        );
    }
    assert!(
        temp.path().join("fresh-registry.bin").is_file(),
        "fresh scanner registry was not created"
    );
}

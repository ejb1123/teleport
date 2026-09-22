#![cfg(target_os = "linux")]

fn mesa_runtime_fixture(
    system_driver: bool,
    nvidia: bool,
    overrides: &[(&str, &str)],
) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    let system_path = temp.path().join("system-driver");
    let nvidia_path = temp.path().join("nvidia-module");
    if system_driver {
        std::fs::create_dir(&system_path).unwrap();
    }
    if nvidia {
        std::fs::create_dir(&nvidia_path).unwrap();
    }
    // Exercise the exact shipped shell logic without modifying /run or /sys.
    let script = include_str!("../nix/mesa-runtime.sh")
        .replace("/run/opengl-driver/lib", system_path.to_str().unwrap())
        .replace("/sys/module/nvidia", nvidia_path.to_str().unwrap())
        .replace("@mesa@", "/nix/store/teleport-test-mesa");
    let mut command = std::process::Command::new("bash");
    command
        .env_clear()
        .env("LD_LIBRARY_PATH", "/foreign/lib")
        .env("LD_PRELOAD", "")
        .args(["-c", &format!(
            r#"unset LD_LIBRARY_PATH LD_PRELOAD
{script}
printf '%s\n' "${{LD_LIBRARY_PATH-<unset>}}" "${{LD_PRELOAD-<unset>}}" "${{LIBGL_DRIVERS_PATH-<unset>}}" "${{__EGL_VENDOR_LIBRARY_FILENAMES-<unset>}}" "${{__EGL_VENDOR_LIBRARY_DIRS-<unset>}}" "${{LIBGL_ALWAYS_SOFTWARE-<unset>}}"
"#
        )]);
    for (name, value) in overrides {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn mesa_runtime_supplies_only_pinned_drivers_on_non_nixos() {
    assert_eq!(
        mesa_runtime_fixture(false, false, &[]),
        [
            "/nix/store/teleport-test-mesa/lib",
            "<unset>",
            "/nix/store/teleport-test-mesa/lib/dri",
            "/nix/store/teleport-test-mesa/share/glvnd/egl_vendor.d/50_mesa.json",
            "<unset>",
            "<unset>",
        ]
    );
}

#[test]
fn mesa_runtime_leaves_nixos_and_nvidia_stacks_unchanged() {
    for (system_driver, nvidia) in [(true, false), (false, true), (true, true)] {
        assert_eq!(
            mesa_runtime_fixture(system_driver, nvidia, &[]),
            ["<unset>"; 6]
        );
    }
}

#[test]
fn mesa_runtime_preserves_explicit_dri_and_egl_overrides() {
    for egl_variable in [
        "__EGL_VENDOR_LIBRARY_FILENAMES",
        "__EGL_VENDOR_LIBRARY_DIRS",
    ] {
        // An explicitly empty override also must not be replaced.
        for value in ["/custom/vendor", ""] {
            let output = mesa_runtime_fixture(
                false,
                false,
                &[("LIBGL_DRIVERS_PATH", "/custom/dri"), (egl_variable, value)],
            );
            assert_eq!(output[0], "/nix/store/teleport-test-mesa/lib");
            assert_eq!(output[1], "<unset>");
            assert_eq!(output[2], "/custom/dri");
            if egl_variable == "__EGL_VENDOR_LIBRARY_FILENAMES" {
                assert_eq!(output[3], value);
                assert_eq!(output[4], "<unset>");
            } else {
                assert_eq!(output[3], "<unset>");
                assert_eq!(output[4], value);
            }
            assert_eq!(output[5], "<unset>");
        }
    }
}

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

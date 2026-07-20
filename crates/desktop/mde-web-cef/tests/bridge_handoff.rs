//! Process-level guard for the Chromium helper -> native bridge handoff.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn render_once_hands_runtime_contract_to_the_bridge() {
    let root = temp_root("mde-web-cef-handoff");
    let runtime = root.join("cef");
    let bridge = root.join("bridge.sh");
    let log = root.join("bridge.log");
    let registry = root.join("allowlist.env");
    let extension_dir = root.join("lastpass");
    fs::create_dir_all(runtime.join("Release")).expect("release dir");
    fs::create_dir_all(runtime.join("Resources")).expect("resources dir");
    fs::create_dir_all(&extension_dir).expect("extension dir");
    fs::write(
        extension_dir.join("manifest.json"),
        b"{\"manifest_version\":3,\"name\":\"LastPass\",\"version\":\"4.130.0\",\"permissions\":[\"storage\",\"tabs\"]}",
    )
    .expect("extension manifest");
    fs::write(runtime.join("Release/libcef.so"), b"fake libcef").expect("libcef");
    fs::write(runtime.join("Resources/icudtl.dat"), b"icu").expect("icu");
    fs::write(runtime.join("Resources/resources.pak"), b"pak").expect("pak");
    fs::write(
        &registry,
        "            [extension.hdokiejnpimakedhajhdlcegeplioahd]\n\
             name = \"LastPass\"\n\
             version = \"4.130.0\"\n\
             path = \"lastpass\"\n\
             permissions = storage,tabs\n\
             power_sideload = true\n",
    )
    .expect("extension registry");
    fs::write(
        &bridge,
        format!(
            "#!/bin/sh\n\
             printf 'argv=%s %s %s\\n' \"$1\" \"$2\" \"$3\" > {log}\n\
             printf 'root=%s\\n' \"$MDE_CEF_BRIDGE_ROOT\" >> {log}\n\
             printf 'lib=%s\\n' \"$MDE_CEF_BRIDGE_LIBCEF\" >> {log}\n\
             printf 'release=%s\\n' \"$MDE_CEF_BRIDGE_RELEASE_DIR\" >> {log}\n\
             printf 'resources=%s\\n' \"$MDE_CEF_BRIDGE_RESOURCES_DIR\" >> {log}\n\
             printf 'extensions=%s\\n' \"$MDE_CEF_BRIDGE_EXTENSIONS\" >> {log}\n\
             printf 'extension_registry=%s\\n' \"$MDE_CEF_BRIDGE_EXTENSION_REGISTRY\" >> {log}\n\
             printf 'extension_power=%s\\n' \"$MDE_CEF_EXTENSION_POWER_MODE\" >> {log}\n\
             printf 'ld=%s\\n' \"$LD_LIBRARY_PATH\" >> {log}\n\
             exit 77\n",
            log = log.display()
        ),
    )
    .expect("bridge script");
    fs::set_permissions(&bridge, fs::Permissions::from_mode(0o755)).expect("chmod bridge");

    let output = Command::new(env!("CARGO_BIN_EXE_mde-web-cef"))
        .arg("render-once")
        .arg("--url")
        .arg("https://example.com/")
        .env("MDE_CEF_ROOT", &runtime)
        .env("MDE_CEF_BRIDGE_BIN", &bridge)
        .env("MDE_CEF_EXTENSION_REGISTRY", &registry)
        .env("MDE_CEF_EXTENSION_POWER_MODE", "true")
        .env("LD_LIBRARY_PATH", "/usr/lib64")
        .output()
        .expect("run helper");

    assert_eq!(output.status.code(), Some(77));
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("CEF_OK"), "{stdout}");
    assert!(stdout.contains("CEF_LAUNCH"), "{stdout}");
    assert!(stdout.contains("CEF_EXTENSIONS_READY"), "{stdout}");
    assert!(stdout.contains("CEF_EXTENSIONS_SKIPPED_V1"), "{stdout}");
    assert!(stdout.contains("extensions=0"), "{stdout}");
    let bridge_log = fs::read_to_string(&log).expect("bridge log");
    assert!(bridge_log.contains("argv=render-once --url https://example.com/"));
    assert!(bridge_log.contains(&format!("root={}", runtime.display())));
    assert!(bridge_log.contains(&format!(
        "lib={}",
        runtime.join("Release/libcef.so").display()
    )));
    assert!(bridge_log.contains(&format!("release={}", runtime.join("Release").display())));
    assert!(bridge_log.contains(&format!(
        "resources={}",
        runtime.join("Resources").display()
    )));
    assert!(log_has_line(&bridge_log, "extensions="), "{bridge_log}");
    assert!(
        log_has_line(&bridge_log, "extension_registry="),
        "{bridge_log}"
    );
    assert!(bridge_log.contains("extension_power=true"));
    assert!(bridge_log.contains(&format!("ld={}", runtime.join("Release").display())));
    assert!(bridge_log.contains("/usr/lib64"));

    let lab_output = Command::new(env!("CARGO_BIN_EXE_mde-web-cef"))
        .arg("render-once")
        .arg("--url")
        .arg("https://example.com/")
        .env("MDE_CEF_ROOT", &runtime)
        .env("MDE_CEF_BRIDGE_BIN", &bridge)
        .env("MDE_CEF_EXTENSION_REGISTRY", &registry)
        .env("MDE_CEF_EXTENSION_POWER_MODE", "true")
        .env("MDE_CEF_WEBEXTENSIONS_LAB", "true")
        .env("LD_LIBRARY_PATH", "/usr/lib64")
        .output()
        .expect("run helper with webextensions lab");

    assert_eq!(lab_output.status.code(), Some(77));
    let lab_stdout = String::from_utf8(lab_output.stdout).expect("stdout utf8");
    assert!(lab_stdout.contains("CEF_EXTENSIONS_READY"), "{lab_stdout}");
    assert!(lab_stdout.contains("extensions=1"), "{lab_stdout}");
    let lab_log = fs::read_to_string(&log).expect("bridge lab log");
    assert!(lab_log.contains(&format!("extensions={}", extension_dir.display())));
    assert!(lab_log.contains(&format!("extension_registry={}", registry.display())));
    assert!(lab_log.contains("extension_power=true"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn packaged_smoke_extension_registry_reaches_the_bridge() {
    let root = temp_root("mde-web-cef-smoke-handoff");
    let runtime = root.join("cef");
    let bridge = root.join("bridge.sh");
    let log = root.join("bridge.log");
    fs::create_dir_all(runtime.join("Release")).expect("release dir");
    fs::create_dir_all(runtime.join("Resources")).expect("resources dir");
    fs::write(runtime.join("Release/libcef.so"), b"fake libcef").expect("libcef");
    fs::write(runtime.join("Resources/icudtl.dat"), b"icu").expect("icu");
    fs::write(runtime.join("Resources/resources.pak"), b"pak").expect("pak");
    fs::write(
        &bridge,
        format!(
            "#!/bin/sh\n\
             printf 'extensions=%s\\n' \"$MDE_CEF_BRIDGE_EXTENSIONS\" > {log}\n\
             printf 'extension_registry=%s\\n' \"$MDE_CEF_BRIDGE_EXTENSION_REGISTRY\" >> {log}\n\
             printf 'extension_power=%s\\n' \"$MDE_CEF_EXTENSION_POWER_MODE\" >> {log}\n\
             exit 77\n",
            log = log.display()
        ),
    )
    .expect("bridge script");
    fs::set_permissions(&bridge, fs::Permissions::from_mode(0o755)).expect("chmod bridge");

    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repo root");
    let registry = repo_root.join("packaging/browser/webextensions-smoke.env");
    let smoke_extension = repo_root.join("packaging/browser/smoke-extension");
    let output = Command::new(env!("CARGO_BIN_EXE_mde-web-cef"))
        .arg("render-once")
        .arg("--url")
        .arg("https://example.com/")
        .env("MDE_CEF_ROOT", &runtime)
        .env("MDE_CEF_BRIDGE_BIN", &bridge)
        .env("MDE_CEF_EXTENSION_REGISTRY", &registry)
        .env("MDE_CEF_EXTENSION_POWER_MODE", "true")
        .output()
        .expect("run helper");

    assert_eq!(output.status.code(), Some(77));
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("CEF_EXTENSIONS_READY"), "{stdout}");
    assert!(stdout.contains("CEF_EXTENSIONS_SKIPPED_V1"), "{stdout}");
    assert!(stdout.contains("extensions=0"), "{stdout}");
    let bridge_log = fs::read_to_string(&log).expect("bridge log");
    assert!(log_has_line(&bridge_log, "extensions="), "{bridge_log}");
    assert!(
        log_has_line(&bridge_log, "extension_registry="),
        "{bridge_log}"
    );
    assert!(bridge_log.contains("extension_power=true"));

    let lab_output = Command::new(env!("CARGO_BIN_EXE_mde-web-cef"))
        .arg("render-once")
        .arg("--url")
        .arg("https://example.com/")
        .env("MDE_CEF_ROOT", &runtime)
        .env("MDE_CEF_BRIDGE_BIN", &bridge)
        .env("MDE_CEF_EXTENSION_REGISTRY", &registry)
        .env("MDE_CEF_EXTENSION_POWER_MODE", "true")
        .env("MDE_CEF_WEBEXTENSIONS_LAB", "true")
        .output()
        .expect("run helper with webextensions lab");

    assert_eq!(lab_output.status.code(), Some(77));
    let lab_stdout = String::from_utf8(lab_output.stdout).expect("stdout utf8");
    assert!(lab_stdout.contains("CEF_EXTENSIONS_READY"), "{lab_stdout}");
    assert!(lab_stdout.contains("extensions=1"), "{lab_stdout}");
    let lab_log = fs::read_to_string(&log).expect("bridge lab log");
    assert!(lab_log.contains(&format!("extensions={}", smoke_extension.display())));
    assert!(lab_log.contains(&format!("extension_registry={}", registry.display())));
    assert!(lab_log.contains("extension_power=true"));
    let _ = fs::remove_dir_all(root);
}

fn temp_root(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()))
}

fn log_has_line(log: &str, expected: &str) -> bool {
    log.lines().any(|line| line == expected)
}

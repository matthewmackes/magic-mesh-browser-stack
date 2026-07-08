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
    fs::create_dir_all(runtime.join("Release")).expect("release dir");
    fs::create_dir_all(runtime.join("Resources")).expect("resources dir");
    fs::write(runtime.join("Release/libcef.so"), b"fake libcef").expect("libcef");
    fs::write(runtime.join("Resources/icudtl.dat"), b"icu").expect("icu");
    fs::write(runtime.join("Resources/resources.pak"), b"pak").expect("pak");
    fs::write(
        &bridge,
        format!(
            "#!/bin/sh\n\
             printf 'argv=%s %s %s\\n' \"$1\" \"$2\" \"$3\" > {log}\n\
             printf 'root=%s\\n' \"$MDE_CEF_BRIDGE_ROOT\" >> {log}\n\
             printf 'lib=%s\\n' \"$MDE_CEF_BRIDGE_LIBCEF\" >> {log}\n\
             printf 'release=%s\\n' \"$MDE_CEF_BRIDGE_RELEASE_DIR\" >> {log}\n\
             printf 'resources=%s\\n' \"$MDE_CEF_BRIDGE_RESOURCES_DIR\" >> {log}\n\
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
        .env("LD_LIBRARY_PATH", "/usr/lib64")
        .output()
        .expect("run helper");

    assert_eq!(output.status.code(), Some(77));
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("CEF_OK"), "{stdout}");
    assert!(stdout.contains("CEF_LAUNCH"), "{stdout}");
    let log = fs::read_to_string(&log).expect("bridge log");
    assert!(log.contains("argv=render-once --url https://example.com/"));
    assert!(log.contains(&format!("root={}", runtime.display())));
    assert!(log.contains(&format!(
        "lib={}",
        runtime.join("Release/libcef.so").display()
    )));
    assert!(log.contains(&format!("release={}", runtime.join("Release").display())));
    assert!(log.contains(&format!(
        "resources={}",
        runtime.join("Resources").display()
    )));
    assert!(log.contains(&format!("ld={}", runtime.join("Release").display())));
    assert!(log.contains("/usr/lib64"));
    let _ = fs::remove_dir_all(root);
}

fn temp_root(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()))
}

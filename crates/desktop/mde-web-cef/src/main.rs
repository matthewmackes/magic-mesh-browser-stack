//! CLI entrypoint for the Chromium/CEF browser helper.

use std::process::Command;
use std::process::ExitCode;

use mde_web_cef::{
    browser_power_mode_enabled, configured_bridge_bin, configured_cef_root,
    configured_extension_registry, configured_widevine_root, detect_extension_registry,
    detect_runtime, detect_widevine, extension_power_mode_enabled, parse_mode, CefLaunchPlan, Mode,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let Some(mode) = parse_mode(args.get(1).map(String::as_str)) else {
        print_usage();
        eprintln!("unknown mode");
        return ExitCode::from(2);
    };
    if mode == Mode::Help {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let runtime = detect_runtime(configured_cef_root());
    let widevine = detect_widevine(configured_widevine_root());
    let extensions = detect_extension_registry(configured_extension_registry());
    let extension_power_mode = extension_power_mode_enabled();
    println!("{}", runtime.status_line());
    println!("{}", widevine.status_line());
    println!("{}", extensions.status_line());
    if browser_power_mode_enabled() {
        println!("CEF_BROWSER_POWER_MODE enabled=1");
    }
    if let Some(line) = extensions.power_mode_gate_line(extension_power_mode) {
        println!("{line}");
    }
    if let Some(line) = extensions.runtime_gate_line() {
        println!("{line}");
    }
    match mode {
        Mode::Probe => {
            if runtime.is_available() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(78)
            }
        }
        Mode::Tab | Mode::RenderOnce | Mode::Warm => run_renderer_mode(
            &runtime,
            &widevine,
            extension_power_mode,
            mode,
            args.iter().skip(2),
        ),
        Mode::Help => ExitCode::SUCCESS,
    }
}

fn run_renderer_mode<'a>(
    runtime: &mde_web_cef::CefRuntime,
    widevine: &mde_web_cef::WidevineCdm,
    extension_power_mode: bool,
    mode: Mode,
    passthrough_args: impl Iterator<Item = &'a String>,
) -> ExitCode {
    if !runtime.is_available() {
        eprintln!("Chromium/CEF engine is gated until the pinned CEF bundle is installed");
        return ExitCode::from(78);
    }

    let extensions = detect_extension_registry(configured_extension_registry());
    let Some(plan) = CefLaunchPlan::new_with_widevine_extensions_and_power_mode(
        runtime,
        widevine,
        &extensions,
        extension_power_mode,
        configured_bridge_bin(),
    ) else {
        eprintln!("Chromium/CEF runtime is present but its launch plan is incomplete");
        return ExitCode::from(78);
    };
    println!("{}", plan.status_line());
    if !plan.bridge_available() {
        eprintln!("{}", plan.missing_bridge_line());
        return ExitCode::from(78);
    }

    let mut command = Command::new(&plan.bridge_bin);
    command
        .arg(mode.as_cli_arg())
        .args(passthrough_args)
        .envs(plan.bridge_env())
        .env(
            "LD_LIBRARY_PATH",
            plan.merged_ld_library_path(std::env::var_os("LD_LIBRARY_PATH")),
        );

    match command.status() {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or_else(|| ExitCode::from(1), ExitCode::from),
        Err(err) => {
            eprintln!(
                "CEF_RENDERER_SPAWN_FAILED bridge={} error={err}",
                plan.bridge_bin.display()
            );
            ExitCode::from(78)
        }
    }
}

fn print_usage() {
    println!(
        "mde-web-cef probe\n\
         mde-web-cef render-once [--url U] [--width W] [--height H]\n\
         mde-web-cef tab --url U [--width W] [--height H]\n\
         mde-web-cef warm [--width W] [--height H]"
    );
}

trait ModeCliArg {
    fn as_cli_arg(self) -> &'static str;
}

impl ModeCliArg for Mode {
    fn as_cli_arg(self) -> &'static str {
        match self {
            Mode::Probe => "probe",
            Mode::Tab => "tab",
            Mode::RenderOnce => "render-once",
            Mode::Warm => "warm",
            Mode::Help => "help",
        }
    }
}

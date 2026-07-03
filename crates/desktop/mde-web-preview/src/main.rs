//! `mde-web-preview` — entrypoint for the sandboxed Servo browser helper
//! (`BOOKMARKS-5`).
//!
//! ```text
//! mde-web-preview render-once [--url U] [--width W] [--height H] [--sandbox]
//! mde-web-preview tab --url U [--width W] [--height H]
//! mde-web-preview warm [--width W] [--height H] [--sandbox]
//! ```
//!
//! `render-once` boots the engine, loads `U` (default `about:blank`), publishes
//! the first painted frame to an internal shm channel, prints `FRAME_OK`, and
//! exits — the headless Definition-of-Done path and the binary self-test;
//! `--sandbox` additionally applies the full OS sandbox first.
//!
//! `tab` is the production per-tab process: it applies the OS sandbox, boots the
//! engine, and continuously publishes frames to the shm channel (whose fd
//! `BOOKMARKS-6` receives over the session socket).
//!
//! `warm` is the one warm helper: it pays Servo's heavy first-init cost up
//! front, then blocks on stdin for the first URL so the first real tab is
//! instant.
//!
//! Zero telemetry: this binary opens no network connection of its own and emits
//! no analytics — only the page the user navigates to is fetched.

use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use mde_web_preview::engine::Engine;
use mde_web_preview::sandbox::{self, SandboxPolicy};
use mde_web_preview::shm::FrameChannel;

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map_or("warm", String::as_str);
    let rest: &[String] = args.get(2..).unwrap_or(&[]);
    match mode {
        "render-once" => render_once(rest),
        "tab" => run_tab(rest),
        "warm" => run_warm(rest),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("unknown mode '{other}'");
        }
    }
}

/// Boot the engine, publish the first frame, report it, and exit.
fn render_once(args: &[String]) -> Result<()> {
    let url = flag(args, "--url").unwrap_or_else(|| "about:blank".to_owned());
    let (width, height) = dimensions(args)?;

    if has_flag(args, "--sandbox") {
        sandbox::apply(SandboxPolicy::tab()).context("apply sandbox")?;
    }

    let channel = FrameChannel::create(width, height).context("create shm channel")?;
    let engine = Engine::new_headless(width, height, &url).context("boot engine")?;
    engine
        .pump_until_frame(&channel, FIRST_FRAME_TIMEOUT)
        .context("render first frame")?;

    // Independently confirm a frame actually landed on the shm channel.
    let frame = channel
        .read_latest()
        .context("no frame on the shm channel after render")?;
    println!(
        "FRAME_OK {}x{} seq={} bytes={}",
        frame.width,
        frame.height,
        channel.sequence(),
        frame.pixels.len()
    );
    Ok(())
}

/// The production per-tab process: sandbox, then serve frames continuously.
fn run_tab(args: &[String]) -> Result<()> {
    let url = flag(args, "--url").context("tab mode requires --url")?;
    let (width, height) = dimensions(args)?;

    // Confine BEFORE touching any web content.
    sandbox::apply(SandboxPolicy::tab()).context("apply sandbox")?;

    let channel = FrameChannel::create(width, height).context("create shm channel")?;
    // The fd BOOKMARKS-6 receives over the session socket (SCM_RIGHTS).
    println!("SHM_FD {}", channel.as_raw_fd());

    let engine = Engine::new_headless(width, height, &url).context("boot engine")?;
    loop {
        let _painted = engine.pump_step(&channel).context("serve frame")?;
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// The warm helper: pre-initialise the engine, then wait for the first URL.
fn run_warm(args: &[String]) -> Result<()> {
    let (width, height) = dimensions(args)?;
    if has_flag(args, "--sandbox") {
        sandbox::apply(SandboxPolicy::tab()).context("apply sandbox")?;
    }

    // Pay the heavy first-init cost now (lazy first-launch amortisation): boot
    // on a blank page so SpiderMonkey/WebRender are warm.
    let channel = FrameChannel::create(width, height).context("create shm channel")?;
    let engine = Engine::new_headless(width, height, "about:blank").context("pre-warm engine")?;
    let _ = engine.pump_until_frame(&channel, FIRST_FRAME_TIMEOUT);

    eprintln!("mde-web-preview: warm helper ready; send a URL on stdin");
    let mut line = String::new();
    if std::io::stdin()
        .read_line(&mut line)
        .context("read warm URL")?
        == 0
    {
        return Ok(()); // stdin closed, nothing to do
    }
    let target = line.trim();
    if !target.is_empty() {
        engine.load(target).context("navigate warm helper")?;
        loop {
            let _ = engine.pump_step(&channel)?;
            std::thread::sleep(Duration::from_millis(8));
        }
    }
    Ok(())
}

/// Read `--width` / `--height`, defaulting sensibly.
fn dimensions(args: &[String]) -> Result<(u32, u32)> {
    let width = flag_u32(args, "--width")?.unwrap_or(DEFAULT_WIDTH);
    let height = flag_u32(args, "--height")?.unwrap_or(DEFAULT_HEIGHT);
    if width == 0 || height == 0 {
        bail!("width/height must be non-zero");
    }
    Ok((width, height))
}

/// The value following `name`, if present.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// A `u32` flag value, if present and parseable.
fn flag_u32(args: &[String], name: &str) -> Result<Option<u32>> {
    flag(args, name)
        .map(|v| {
            v.parse::<u32>()
                .with_context(|| format!("{name} must be a number"))
        })
        .transpose()
}

/// Whether a bare flag is present.
fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn print_usage() {
    let mut out = std::io::stderr();
    let _ = writeln!(
        out,
        "mde-web-preview — sandboxed Servo browser helper (BOOKMARKS-5)\n\
         \n\
         USAGE:\n\
         \x20 mde-web-preview render-once [--url U] [--width W] [--height H] [--sandbox]\n\
         \x20 mde-web-preview tab --url U [--width W] [--height H]\n\
         \x20 mde-web-preview warm [--width W] [--height H] [--sandbox]"
    );
}

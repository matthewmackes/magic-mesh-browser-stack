//! `mde-web-preview` — entrypoint for the sandboxed Servo browser helper
//! (`BOOKMARKS-5`).
//!
//! ```text
//! mde-web-preview render-once [--url U] [--width W] [--height H] [--sandbox]
//! mde-web-preview tab --url U [--width W] [--height H]
//! mde-web-preview warm [--width W] [--height H] [--sandbox]
//! ```
//!
//! `render-once` boots the engine, loads `U` (default `about:blank`), pumps
//! until the page has loaded AND its content has composited (BUG-BROWSER-6:
//! the FIRST frame is the pre-content shell-background clear), publishes the
//! newest frame to an internal shm channel, prints `FRAME_OK`, and exits — the
//! headless Definition-of-Done path and the binary self-test; `--sandbox`
//! additionally applies the full OS sandbox first. `MDE_WEB_DEBUG=1` traces
//! every capture (per-frame distinct/luma stats) to stderr.
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
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use mde_web_preview::engine::{frame_stats, Engine};
use mde_web_preview::sandbox::{self, SandboxPolicy};
use mde_web_preview::shm::FrameChannel;
use mde_web_preview::sock::{self, RecvOutcome};
use mde_web_preview::wire::{self, ControlMsg, EventMsg};

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the `tab` serve loop waits for a natural first paint before the
/// first-frame watchdog force-emits a frame anyway (so a slow/heavy page cannot
/// leave the shell stuck on "Loading the page…").
const FIRST_FRAME_WATCHDOG: Duration = Duration::from_millis(750);

/// The session socket the shell hands the `tab` child as its stdin (fd 0) — see
/// `mde-web-preview-client`'s `WebSession::spawn`.
const SESSION_SOCKET_FD: std::os::fd::RawFd = 0;

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
    // Wait for load-complete + the content composite, not the first frame — the
    // first frame-ready fires for the initial EMPTY scene (the uniform
    // shell-background clear), before the page's display list ever reaches
    // WebRender (BUG-BROWSER-6).
    engine
        .pump_until_content_frame(&channel, FIRST_FRAME_TIMEOUT)
        .context("render content frame")?;

    // Independently confirm a frame actually landed on the shm channel.
    let frame = channel
        .read_latest()
        .context("no frame on the shm channel after render")?;
    // Render-once is the headless DoD path AND the eyes-on render aid: alongside
    // the geometry, report cheap content stats (distinct byte values + mean luma)
    // so a caller can tell a real render from a blank/white frame without a PNG
    // decoder — useful when deciding whether a Servo-compat follow-up is needed.
    let (distinct, mean_luma) = frame_stats(&frame.pixels);
    println!(
        "FRAME_OK {}x{} seq={} bytes={} distinct={distinct} mean_luma={mean_luma:.1}",
        frame.width,
        frame.height,
        channel.sequence(),
        frame.pixels.len(),
    );

    // Exercise the watchdog path in the real binary (the crate's engine-exercise
    // idiom is the separate-process self-test, since Servo is one-instance-per-
    // process): force a frame with no fresh frame-ready and confirm the shm
    // sequence advances. `FORCE_OK` is the force_emit acceptance signal.
    if engine.force_emit(&channel).context("force emit")? {
        println!("FORCE_OK seq={}", channel.sequence());
    }

    // Deep-debug probes (MDE_WEB_DEBUG only; no-op otherwise) — see the engine.
    engine.debug_content_probe(Duration::from_secs(20));
    Ok(())
}

/// The production per-tab process: sandbox, then speak the BOOKMARKS-6 socket
/// protocol — attach the shm fd, apply the shell's control frames, and signal each
/// delivered frame with a `PaintReady` (the shell goes Live on the first one).
fn run_tab(args: &[String]) -> Result<()> {
    let url = flag(args, "--url").context("tab mode requires --url")?;
    let (width, height) = dimensions(args)?;

    // Confine BEFORE touching any web content. `apply` forks (CLONE_NEWPID): all
    // socket + channel work below runs in the CONFINED CHILD, so the AttachFrame fd
    // send targets the right process (a pre-fork send would ride the supervisor).
    sandbox::apply(SandboxPolicy::tab()).context("apply sandbox")?;

    // The shell handed us the session socket as our stdin (fd 0). Take sole
    // ownership of it — nothing else in `tab` mode reads stdin — and drive it
    // non-blocking so the serve loop never stalls on a quiet shell.
    // SAFETY: in `tab` mode fd 0 is the connected `AF_UNIX` session socket the
    // shell passed via `Command::stdin`; we own it exclusively for this process.
    let socket = unsafe { UnixStream::from_raw_fd(SESSION_SOCKET_FD) };
    socket
        .set_nonblocking(true)
        .context("session socket non-blocking")?;

    let channel = FrameChannel::create(width, height).context("create shm channel")?;
    // Hand the shm frame-region fd to the shell ONCE via SCM_RIGHTS, so it maps the
    // reader before any frame — the shell's Live gate needs the mapping in place.
    sock::send_frame_with_fd(
        &socket,
        &EventMsg::AttachFrame.encode(),
        channel.as_raw_fd(),
    )
    .context("attach frame fd")?;

    let engine = Engine::new_headless(width, height, &url).context("boot engine")?;
    // Announce the committed URL so the chrome's address bar reflects it (the
    // ad-filter first-party is (re)anchored on this too, BOOKMARKS-7).
    announce_nav(&socket, &url, false);

    let mut rbuf: Vec<u8> = Vec::new();
    let started = Instant::now();
    let mut first_frame_sent = false;
    loop {
        // (a) Apply every pending control frame the shell sent.
        match sock::recv(&socket) {
            Ok(RecvOutcome::Data { bytes, .. }) => {
                rbuf.extend_from_slice(&bytes);
                loop {
                    match wire::take_frame(&mut rbuf) {
                        Ok(Some(payload)) => {
                            if let Ok(msg) = ControlMsg::decode(&payload) {
                                apply_control(&engine, &socket, &msg);
                            }
                        }
                        Ok(None) => break,
                        // A corrupt length prefix from our own shell is not
                        // recoverable — stop serving (the shell reads a crash).
                        Err(_) => return Ok(()),
                    }
                }
            }
            Ok(RecvOutcome::WouldBlock) => {}
            // The shell closed the socket (tab closed) — exit cleanly.
            Ok(RecvOutcome::Eof) | Err(_) => return Ok(()),
        }

        // (b) Spin one step; publish a frame if the engine painted one.
        let painted = engine.pump_step(&channel).context("serve frame")?;

        // (c) First-frame watchdog: if nothing has been delivered yet and the grace
        //     window elapsed, force a frame so a slow/heavy page (which may never
        //     fire a prompt frame-ready) cannot hang the shell on "Loading…". Keyed
        //     on a delivered frame, NEVER on `load_complete()`.
        let forced = if !first_frame_sent && !painted && started.elapsed() >= FIRST_FRAME_WATCHDOG {
            engine.force_emit(&channel).context("watchdog force emit")?
        } else {
            false
        };

        // (d) Signal paint-ready on any delivered frame — the FIRST one makes the
        //     shell go Live and stop showing "Loading the page…".
        if painted || forced {
            first_frame_sent = true;
            // A send failure means the shell is gone; stop serving.
            if sock::send_frame(
                &socket,
                &EventMsg::PaintReady {
                    seq: channel.sequence(),
                }
                .encode(),
            )
            .is_err()
            {
                return Ok(());
            }
        }

        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Apply one control frame from the shell to the engine. Navigation is wired to the
/// engine's existing methods; zoom/find/force-dark/audio-mute use the helper's
/// DOM script seam. `Stop`/`Resize`/`Input`/`ResourceVerdict`/`CosmeticFilters`
/// are decoded (so the framed stream stays in sync) but not yet acted on — Servo
/// currently exposes no real cancel-load, live-resize, input-injection, or
/// helper-side ad-filter hook here.
fn apply_control(engine: &Engine, socket: &UnixStream, msg: &ControlMsg) {
    match msg {
        ControlMsg::Load(url) => {
            if engine.load(url).is_ok() {
                announce_nav(socket, url, true);
            }
        }
        ControlMsg::Reload => engine.reload(),
        ControlMsg::Back => engine.go_back(1),
        ControlMsg::Forward => engine.go_forward(1),
        ControlMsg::SetZoom { percent } => engine.set_zoom(*percent),
        ControlMsg::FindInPage { query, backwards } => engine.find_in_page(query, *backwards),
        ControlMsg::ClearFind => engine.clear_find(),
        ControlMsg::SetAudioMuted { muted } => engine.set_audio_muted(*muted),
        ControlMsg::SetForceDark { enabled } => engine.set_force_dark(*enabled),
        ControlMsg::SetReaderMode { enabled } => engine.set_reader_mode(*enabled),
        ControlMsg::PrintPage => engine.print_page(),
        ControlMsg::Stop
        | ControlMsg::Resize { .. }
        | ControlMsg::Input(_)
        | ControlMsg::ResourceVerdict { .. }
        | ControlMsg::CosmeticFilters(_)
        | ControlMsg::SavePdf { .. } => {}
    }
}

/// Push a best-effort nav-state so the chrome's address bar shows the committed
/// URL. History-edge flags are unknown to this minimal serve loop, so they are
/// reported conservatively as `false`; `loading` toggles on navigation.
fn announce_nav(socket: &UnixStream, url: &str, loading: bool) {
    let _ = sock::send_frame(
        socket,
        &EventMsg::NavState {
            can_back: false,
            can_forward: false,
            loading,
            url: url.to_owned(),
        }
        .encode(),
    );
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

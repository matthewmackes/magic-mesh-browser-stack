//! Standalone browser wire-verification harness (BROWSER verify).
//!
//! Spawns the sandboxed browser helper EXACTLY as the shell's `WebSession` does
//! (a socketpair on the helper's stdin, `tab` mode), then polls the session socket
//! and prints each display/load-handler callback as it arrives OVER THE WIRE:
//!
//!   * `on_address_change`      → `NavState.url` changes
//!   * `on_loading_state_change`→ `NavState.{loading,can_back,can_forward}`
//!   * `on_title_change`        → `title()` changes
//!   * `on_favicon_urlchange`   → `favicon()` bytes arrive
//!   * `on_paint_ready`         → a shm frame decoded through `WebSession`
//!
//! This is the honest end-to-end proof that a real Browser helper responds over
//! the same AF_UNIX wire the shell consumes, with NO shell and NO reboot. The
//! binary name is historical (`cef-verify`) because the first live use was CEF
//! display/load verification, but the harness also works against `mde-web-preview`
//! (Servo), can prove mouse+keyboard response with `MDE_BROWSER_VERIFY_INPUT=1`,
//! and can prove click-driven link navigation with
//! `MDE_BROWSER_VERIFY_LINK_NAV=1`.
//!
//! Usage: `cef-verify <helper_bin> <url> [seconds]`
//!   e.g. `cef-verify /usr/bin/mde-web-cef https://example.com/ 20`
//!   e.g. `MDE_BROWSER_VERIFY_INPUT=1 cef-verify /usr/bin/mde-web-preview`
//!   e.g. `MDE_BROWSER_VERIFY_LINK_NAV=1 cef-verify /usr/bin/mde-web-cef`
//!   e.g. `MDE_BROWSER_VERIFY_IDLE_MEDIA=1 cef-verify /usr/bin/mde-web-cef "" 70`

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::time::{Duration, Instant};

use mde_web_preview_client::egui::{self, pos2};
use mde_web_preview_client::session::{SpawnSpec, WebSession};

fn main() {
    let mode = VerifyMode::from_env();
    let args = VerifyArgs::parse(std::env::args().skip(1), mode);

    let spec = SpawnSpec {
        helper_bin: args.helper.clone().into(),
        env: Vec::new(),
        url: args.url.clone(),
        width: 1280,
        height: 800,
    };
    println!(
        "VERIFY spawn helper={} mode={} url={} budget={}s",
        args.helper,
        mode.label(),
        args.url,
        args.secs
    );
    let mut sess = match WebSession::spawn(&spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("VERIFY spawn FAILED: {e}");
            std::process::exit(2);
        }
    };

    let mut last_url = String::new();
    let mut last_title = String::new();
    let mut favicon_seen = false;
    let mut nav_events = 0u32;
    let mut title_events = 0u32;
    let mut frame_events = 0u32;
    let input_probe = mode == VerifyMode::Input;
    let link_nav_probe = mode == VerifyMode::LinkNavigation;
    let idle_media_probe = mode == VerifyMode::IdleMedia;
    // Servo's minimal helper currently does not publish dynamic title changes,
    // so page-text polling is the cross-engine observable for input response.
    let page_text_input_probe = input_probe || env_flag("MDE_BROWSER_VERIFY_PAGE_TEXT_INPUT");
    let mut input_probe_state = InputProbeState::new(page_text_input_probe);
    let mut link_nav_probe_state = LinkNavigationProbeState::new(args.url.clone());
    let mut idle_probe_state = IdleMediaProbeState::new(
        idle_media_min_seconds(args.secs),
        idle_media_min_signatures(),
    );
    let mut last_media_playing = None;
    let started = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(args.secs);
    while Instant::now() < deadline {
        sess.poll();
        if input_probe {
            input_probe_state.drain_page_text(&mut sess);
            input_probe_state.maybe_request_page_text(&mut sess);
        }
        if let Some(frame) = sess.take_frame() {
            if idle_media_probe {
                idle_probe_state.observe_frame(&frame, started.elapsed());
            }
            println!(
                "VERIFY on_paint_ready view={}x{} pixels={}",
                frame.size[0],
                frame.size[1],
                frame.pixels.len()
            );
            frame_events += 1;
        }
        let nav = sess.nav();
        if nav.url != last_url {
            println!(
                "VERIFY on_address_change url={} loading={} back={} fwd={}",
                nav.url, nav.loading, nav.can_back, nav.can_forward
            );
            last_url = nav.url.clone();
            nav_events += 1;
        }
        let title = sess.title();
        if title != last_title {
            println!("VERIFY on_title_change title={title}");
            last_title = title.to_string();
            title_events += 1;
        }
        if !favicon_seen {
            if let Some(bytes) = sess.favicon() {
                println!("VERIFY on_favicon_urlchange bytes={}", bytes.len());
                favicon_seen = true;
            }
        }
        if idle_media_probe {
            if let Some(metadata) = sess.media_metadata() {
                let playing = media_metadata_reports_playing(&metadata.body);
                idle_probe_state.observe_media_metadata(playing);
                if last_media_playing != Some(playing) {
                    println!(
                        "VERIFY media_metadata bytes={} playing={playing}",
                        metadata.body.len()
                    );
                    last_media_playing = Some(playing);
                }
            }
        }
        if input_probe {
            drive_input_probe(&mut sess, frame_events, &mut input_probe_state);
        }
        if link_nav_probe {
            drive_link_navigation_probe(&mut sess, frame_events, &mut link_nav_probe_state);
        }
        if input_probe
            && nav_events > 0
            && frame_events > 0
            && input_probe_state.is_complete(sess.title())
        {
            break;
        }
        if idle_media_probe && nav_events > 0 && idle_probe_state.is_complete(started.elapsed()) {
            break;
        }
        if link_nav_probe
            && nav_events > 0
            && frame_events > 0
            && link_nav_probe_state.is_complete(&sess.nav().url)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    println!(
        "VERIFY DONE nav_events={nav_events} title_events={title_events} frame_events={frame_events} favicon={favicon_seen} final_url={} final_title={}",
        sess.nav().url,
        sess.title(),
    );
    let input_ok = !input_probe || input_probe_state.is_complete(sess.title());
    let link_nav_ok = !link_nav_probe || link_nav_probe_state.is_complete(&sess.nav().url);
    let idle_media_ok = !idle_media_probe || idle_probe_state.is_complete(started.elapsed());
    if nav_events > 0 && frame_events > 0 && input_ok && link_nav_ok && idle_media_ok {
        if input_probe {
            println!("VERIFY RESULT=PASS display/load/input response observed over the wire");
        } else if link_nav_probe {
            println!(
                "VERIFY RESULT=PASS click-driven link navigation observed over the wire ({})",
                link_nav_probe_state.summary(&sess.nav().url)
            );
        } else if idle_media_probe {
            println!(
                "VERIFY RESULT=PASS idle media advanced without pointer input ({})",
                idle_probe_state.summary()
            );
        } else {
            println!(
                "VERIFY RESULT=PASS display/load handler fired and a frame arrived over the wire"
            );
        }
    } else {
        if link_nav_probe {
            println!(
                "VERIFY RESULT=FAIL missing NavState, frame, or click-driven link navigation ({})",
                link_nav_probe_state.summary(&sess.nav().url)
            );
        } else if idle_media_probe {
            println!(
                "VERIFY RESULT=FAIL missing NavState, frame, or no-input idle media progress ({})",
                idle_probe_state.summary()
            );
        } else {
            println!("VERIFY RESULT=FAIL missing NavState, frame, or input response over the wire");
        }
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }
    let _ = std::io::stdout().flush();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerifyMode {
    Display,
    Input,
    LinkNavigation,
    IdleMedia,
}

impl VerifyMode {
    fn from_env() -> Self {
        if env_flag("MDE_BROWSER_VERIFY_IDLE_MEDIA") {
            Self::IdleMedia
        } else if env_flag("MDE_BROWSER_VERIFY_LINK_NAV")
            || env_flag("MDE_BROWSER_VERIFY_LINK_NAVIGATION")
        {
            Self::LinkNavigation
        } else if env_flag("MDE_CEF_VERIFY_INPUT") || env_flag("MDE_BROWSER_VERIFY_INPUT") {
            Self::Input
        } else {
            Self::Display
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Input => "input",
            Self::LinkNavigation => "link-navigation",
            Self::IdleMedia => "idle-media",
        }
    }

    const fn default_budget_secs(self) -> u64 {
        match self {
            Self::Display | Self::Input | Self::LinkNavigation => 20,
            Self::IdleMedia => 70,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct VerifyArgs {
    helper: String,
    url: String,
    secs: u64,
}

impl VerifyArgs {
    fn parse(args: impl IntoIterator<Item = String>, mode: VerifyMode) -> Self {
        let mut args = args.into_iter();
        let helper = args
            .next()
            .filter(|arg| !arg.trim().is_empty())
            .unwrap_or_else(|| "/usr/bin/mde-web-cef".to_string());
        let url = args
            .next()
            .filter(|arg| !arg.trim().is_empty())
            .unwrap_or_else(|| default_verify_url(mode));
        let secs = args
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| mode.default_budget_secs());
        Self { helper, url, secs }
    }
}

fn default_verify_url(mode: VerifyMode) -> String {
    match mode {
        VerifyMode::Display => "about:blank".to_string(),
        VerifyMode::Input => input_probe_url(),
        VerifyMode::LinkNavigation => link_navigation_probe_url(),
        VerifyMode::IdleMedia => idle_media_probe_url(),
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputProbeStep {
    WaitingForFrame,
    SentPointer,
    SentKey,
    SentText,
}

#[derive(Debug)]
struct InputProbeState {
    step: InputProbeStep,
    page_text: bool,
    last_page_text: String,
    next_page_text_id: u64,
    last_page_text_request: Instant,
}

impl InputProbeState {
    fn new(page_text: bool) -> Self {
        Self {
            step: InputProbeStep::WaitingForFrame,
            page_text,
            last_page_text: String::new(),
            next_page_text_id: 1,
            last_page_text_request: Instant::now() - Duration::from_secs(1),
        }
    }

    fn drain_page_text(&mut self, sess: &mut WebSession) {
        for event in sess.drain_page_text_events() {
            println!(
                "VERIFY on_page_text id={} bytes={} text={}",
                event.id,
                event.text.len(),
                compact_text(&event.text),
            );
            self.last_page_text = event.text;
        }
    }

    fn maybe_request_page_text(&mut self, sess: &mut WebSession) {
        if !self.page_text || self.is_complete(sess.title()) {
            return;
        }
        if self.last_page_text_request.elapsed() < Duration::from_millis(200) {
            return;
        }
        let id = self.next_page_text_id;
        self.next_page_text_id = self.next_page_text_id.saturating_add(1);
        self.last_page_text_request = Instant::now();
        sess.request_page_text(id, 2048);
        println!("VERIFY page_text_probe_requested id={id}");
    }

    fn saw_initial(&self, title: &str) -> bool {
        title.contains("p0") || self.last_page_text.contains("P:0")
    }

    fn saw_pointer(&self, title: &str) -> bool {
        title.contains("p1") || self.last_page_text.contains("P:1")
    }

    fn saw_key(&self, title: &str) -> bool {
        title.contains("k1") || self.last_page_text.contains("K:1")
    }

    fn saw_text(&self, title: &str) -> bool {
        title.contains("tm") || self.last_page_text.contains("T:m")
    }

    fn is_complete(&self, title: &str) -> bool {
        self.step == InputProbeStep::SentText
            && self.saw_pointer(title)
            && self.saw_key(title)
            && self.saw_text(title)
    }
}

fn drive_input_probe(sess: &mut WebSession, frame_events: u32, state: &mut InputProbeState) {
    match state.step {
        InputProbeStep::WaitingForFrame if frame_events > 0 && state.saw_initial(sess.title()) => {
            send_pointer_probe(sess);
            state.step = InputProbeStep::SentPointer;
        }
        InputProbeStep::SentPointer if state.saw_pointer(sess.title()) => {
            send_key_probe(sess);
            state.step = InputProbeStep::SentKey;
        }
        InputProbeStep::SentKey if state.saw_key(sess.title()) => {
            send_text_probe(sess);
            state.step = InputProbeStep::SentText;
        }
        _ => {}
    }
}

#[derive(Debug)]
struct LinkNavigationProbeState {
    source_url: String,
    sent_click: bool,
}

impl LinkNavigationProbeState {
    fn new(source_url: String) -> Self {
        Self {
            source_url,
            sent_click: false,
        }
    }

    fn is_complete(&self, current_url: &str) -> bool {
        self.sent_click
            && current_url != self.source_url
            && current_url.contains(LINK_NAVIGATION_TARGET_MARKER)
    }

    fn summary(&self, current_url: &str) -> String {
        format!(
            "click_sent={} source_unchanged={} final_has_marker={} final_url={}",
            self.sent_click,
            current_url == self.source_url,
            current_url.contains(LINK_NAVIGATION_TARGET_MARKER),
            current_url,
        )
    }
}

fn drive_link_navigation_probe(
    sess: &mut WebSession,
    frame_events: u32,
    state: &mut LinkNavigationProbeState,
) {
    if state.sent_click || frame_events == 0 {
        return;
    }
    send_link_navigation_probe(sess);
    state.sent_click = true;
}

fn compact_text(text: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
        if out.len() >= 240 {
            out.push_str("...");
            break;
        }
    }
    out.trim().to_owned()
}

#[derive(Debug)]
struct IdleMediaProbeState {
    min_seconds: u64,
    min_signatures: usize,
    signatures: BTreeSet<u64>,
    last_signature: Option<u64>,
    changed_frames: u32,
    first_change_at: Option<Duration>,
    last_change_at: Option<Duration>,
    saw_playing_media: bool,
}

impl IdleMediaProbeState {
    fn new(min_seconds: u64, min_signatures: usize) -> Self {
        Self {
            min_seconds,
            min_signatures,
            signatures: BTreeSet::new(),
            last_signature: None,
            changed_frames: 0,
            first_change_at: None,
            last_change_at: None,
            saw_playing_media: false,
        }
    }

    fn observe_frame(&mut self, frame: &egui::ColorImage, elapsed: Duration) {
        let signature = animated_frame_signature(frame);
        self.signatures.insert(signature);
        if self.last_signature.is_some_and(|last| last != signature) {
            self.changed_frames = self.changed_frames.saturating_add(1);
            self.first_change_at.get_or_insert(elapsed);
            self.last_change_at = Some(elapsed);
        }
        self.last_signature = Some(signature);
    }

    fn observe_media_metadata(&mut self, playing: bool) {
        self.saw_playing_media |= playing;
    }

    fn is_complete(&self, elapsed: Duration) -> bool {
        elapsed >= Duration::from_secs(self.min_seconds)
            && self.saw_playing_media
            && self.signatures.len() >= self.min_signatures
            && self.changed_frames >= self.min_signatures.saturating_sub(1) as u32
    }

    fn summary(&self) -> String {
        format!(
            "elapsed_target={}s signatures={} changed_frames={} playing_media={} first_change_ms={} last_change_ms={}",
            self.min_seconds,
            self.signatures.len(),
            self.changed_frames,
            self.saw_playing_media,
            self.first_change_at
                .map(|duration| duration.as_millis().to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.last_change_at
                .map(|duration| duration.as_millis().to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
    }
}

fn idle_media_min_seconds(budget_secs: u64) -> u64 {
    let default = if budget_secs >= 65 {
        60
    } else {
        budget_secs.saturating_sub(1).max(1)
    };
    env_u64("MDE_BROWSER_VERIFY_IDLE_MIN_SECONDS", default)
        .min(budget_secs.saturating_sub(1).max(1))
}

fn idle_media_min_signatures() -> usize {
    usize::try_from(env_u64("MDE_BROWSER_VERIFY_IDLE_MIN_FRAMES", 4))
        .unwrap_or(4)
        .max(2)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn animated_frame_signature(frame: &egui::ColorImage) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    frame.size.hash(&mut hasher);
    let width = frame.size[0];
    let height = frame.size[1];
    if width == 0 || height == 0 || frame.pixels.is_empty() {
        return hasher.finish();
    }

    for y in sample_positions(height) {
        for x in sample_positions(width) {
            let idx = y
                .saturating_mul(width)
                .saturating_add(x)
                .min(frame.pixels.len().saturating_sub(1));
            let pixel = frame.pixels[idx];
            pixel.r().hash(&mut hasher);
            pixel.g().hash(&mut hasher);
            pixel.b().hash(&mut hasher);
            pixel.a().hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn sample_positions(size: usize) -> [usize; 5] {
    [
        0,
        size / 4,
        size / 2,
        size.saturating_mul(3) / 4,
        size.saturating_sub(1),
    ]
}

fn media_metadata_reports_playing(body: &str) -> bool {
    let mut rest = body;
    while let Some(idx) = rest.find("\"paused\"") {
        rest = &rest[idx + "\"paused\"".len()..];
        let value = rest.trim_start().strip_prefix(':').map(str::trim_start);
        match value {
            Some(v) if v.starts_with("false") => return true,
            Some(v) if v.starts_with("true") => return false,
            _ => {}
        }
        if rest.is_empty() {
            break;
        }
        rest = &rest[1..];
    }
    false
}

fn send_pointer_probe(sess: &mut WebSession) {
    let pos = pos2(80.0, 80.0);
    let modifiers = egui::Modifiers::default();
    sess.send_input(&egui::Event::PointerMoved(pos), 1.0);
    sess.send_input(
        &egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers,
        },
        1.0,
    );
    sess.send_input(
        &egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers,
        },
        1.0,
    );
    println!("VERIFY input_probe_sent pointer=true");
}

fn send_link_navigation_probe(sess: &mut WebSession) {
    let pos = pos2(96.0, 84.0);
    let modifiers = egui::Modifiers::default();
    sess.send_input(&egui::Event::PointerMoved(pos), 1.0);
    sess.send_input(
        &egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers,
        },
        1.0,
    );
    sess.send_input(
        &egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers,
        },
        1.0,
    );
    println!("VERIFY link_navigation_probe_sent pointer=true");
}

fn send_key_probe(sess: &mut WebSession) {
    let modifiers = egui::Modifiers::default();
    sess.send_input(
        &egui::Event::Key {
            key: egui::Key::M,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        },
        1.0,
    );
    println!("VERIFY input_probe_sent key_down=true");
}

fn send_key_release(sess: &mut WebSession) {
    let modifiers = egui::Modifiers::default();
    sess.send_input(
        &egui::Event::Key {
            key: egui::Key::M,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        },
        1.0,
    );
    println!("VERIFY input_probe_sent key_up=true");
}

fn send_text_probe(sess: &mut WebSession) {
    sess.send_input(&egui::Event::Text("m".to_owned()), 1.0);
    send_key_release(sess);
    println!("VERIFY input_probe_sent text=true mode=key-char");
}

fn input_probe_url() -> String {
    data_url(INPUT_PROBE_HTML)
}

fn link_navigation_probe_url() -> String {
    data_url(LINK_NAVIGATION_PROBE_HTML)
}

fn idle_media_probe_url() -> String {
    data_url(IDLE_MEDIA_PROBE_HTML)
}

fn data_url(html: &str) -> String {
    let mut out = String::from("data:text/html;charset=utf-8,");
    for byte in html.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte))
            }
            b' ' => out.push_str("%20"),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

const INPUT_PROBE_HTML: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>mde-browser-verify-p0-k0-t_</title>
<style>
html,body{margin:0;padding:0;background:#101418;color:#f4f4f4;font:16px sans-serif}
#probe{position:absolute;left:32px;top:48px;width:320px;height:96px}
#typed{position:absolute;left:40px;top:64px;width:220px;height:36px;font:18px sans-serif}
#status{position:absolute;left:40px;top:112px}
</style>
<div id="probe">
  <input id="typed" autocomplete="off" value="" aria-label="Browser verify input">
  <div id="status">P:0 K:0 T:_</div>
</div>
<script>
(function(){
  var state={p:0,k:0,t:"_"};
  var typed=document.getElementById("typed");
  var status=document.getElementById("status");
  function render(){
    document.title="mde-browser-verify-p"+state.p+"-k"+state.k+"-t"+state.t;
    status.textContent="P:"+state.p+" K:"+state.k+" T:"+state.t;
  }
  function focusInput(){ try { typed.focus(); } catch(_e) {} }
  document.addEventListener("pointerdown",function(){ state.p=1; focusInput(); render(); },true);
  document.addEventListener("mousedown",function(){ state.p=1; focusInput(); render(); },true);
  document.addEventListener("keydown",function(e){
    if (e && e.key && e.key.toLowerCase && e.key.toLowerCase()==="m") state.k=1;
    render();
  },true);
  document.addEventListener("keypress",function(e){
    if (e && e.key && e.key.length===1) state.t=e.key.toLowerCase();
    render();
  },true);
  typed.addEventListener("input",function(){
    var value=typed.value || "";
    state.t=(value.slice(-1) || "_").toLowerCase();
    render();
  },true);
  window.addEventListener("load",function(){ focusInput(); render(); },true);
  focusInput();
  render();
})();
</script>
"#;

const LINK_NAVIGATION_TARGET_MARKER: &str = "mde-browser-link-clicked";
const LINK_NAVIGATION_TARGET_URL: &str = "about:blank#mde-browser-link-clicked";

const LINK_NAVIGATION_PROBE_HTML: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>mde-browser-link-source</title>
<style>
html,body{margin:0;padding:0;background:#fff;color:#202124;font:16px sans-serif}
a{position:absolute;left:40px;top:52px;width:360px;height:64px;display:flex;align-items:center;justify-content:center;border:2px solid #1a73e8;border-radius:8px;color:#1a73e8;text-decoration:none;font-weight:700}
#status{position:absolute;left:40px;top:136px;color:#5f6368}
</style>
<a id="target" href="about:blank#mde-browser-link-clicked">Open navigation target</a>
<div id="status">Ready for click navigation proof</div>
"#;

const IDLE_MEDIA_PROBE_HTML: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>mde-browser-idle-media-f0</title>
<style>
html,body{margin:0;overflow:hidden;background:#05080c;color:#fff}
canvas{position:absolute;inset:0;width:100vw;height:100vh}
video{position:absolute;right:8px;bottom:8px;width:1px;height:1px;opacity:.01}
</style>
<canvas id=c width=960 height=540></canvas>
<video id=v autoplay muted playsinline></video>
<script>
(function(){
  var c=document.getElementById("c"),v=document.getElementById("v"),x=c.getContext("2d"),f=0;
  function play(){try{var p=v.play();if(p&&p.catch)p.catch(function(){});}catch(e){}}
  try{if(c.captureStream){v.srcObject=c.captureStream(30);v.muted=true;play();}}catch(e){}
  function draw(){
    f++;
    x.fillStyle="hsl("+(f%360)+",82%,44%)";
    x.fillRect(0,0,c.width,c.height);
    x.fillStyle="#fff";
    x.font="700 58px sans-serif";
    x.fillText("MDE idle media "+f,42,92);
    x.fillStyle="rgba(0,0,0,.34)";
    x.fillRect((f*17)%c.width,150,190,190);
    document.title="mde-browser-idle-media-f"+f;
    requestAnimationFrame(draw)
  }
  v.addEventListener("canplay",play,true);
  v.addEventListener("pause",play,true);
  setInterval(play,500);
  draw();
})();
</script>
"##;

#[cfg(test)]
mod tests {
    use super::{
        animated_frame_signature, data_url, idle_media_probe_url, input_probe_url,
        link_navigation_probe_url, media_metadata_reports_playing, sample_positions,
        LinkNavigationProbeState, VerifyArgs, VerifyMode, LINK_NAVIGATION_TARGET_MARKER,
        LINK_NAVIGATION_TARGET_URL,
    };
    use mde_web_preview_client::egui::{Color32, ColorImage};

    #[test]
    fn input_probe_url_is_a_self_contained_data_page_with_expected_markers() {
        let url = input_probe_url();
        assert!(url.starts_with("data:text/html;charset=utf-8,"));
        assert!(url.contains("mde-browser-verify-p0-k0-t_"));
        assert!(url.contains("P%3A0%20K%3A0%20T%3A_"));
        assert!(url.contains("pointerdown"));
        assert!(url.contains("keydown"));
        assert!(url.contains("input"));
    }

    #[test]
    fn data_url_percent_encodes_html_without_losing_ascii_markers() {
        assert_eq!(
            data_url("<title>x y</title>"),
            "data:text/html;charset=utf-8,%3Ctitle%3Ex%20y%3C%2Ftitle%3E"
        );
    }

    #[test]
    fn link_navigation_probe_url_has_a_fixed_offline_click_target() {
        let url = link_navigation_probe_url();
        assert!(url.starts_with("data:text/html;charset=utf-8,"));
        assert!(url.contains("mde-browser-link-source"));
        assert!(url.contains("Open%20navigation%20target"));
        assert!(url.contains("href%3D%22about%3Ablank%23mde-browser-link-clicked%22"));
    }

    #[test]
    fn args_default_to_link_navigation_probe_page_when_link_mode_has_no_url() {
        let args = VerifyArgs::parse(
            vec!["/usr/bin/mde-web-cef".to_owned()],
            VerifyMode::LinkNavigation,
        );

        assert_eq!(args.helper, "/usr/bin/mde-web-cef");
        assert!(args.url.starts_with("data:text/html;charset=utf-8,"));
        assert!(args.url.contains("mde-browser-link-source"));
        assert_eq!(args.secs, 20);
    }

    #[test]
    fn link_navigation_probe_requires_a_click_and_a_changed_target_url() {
        let mut state = LinkNavigationProbeState::new(link_navigation_probe_url());

        assert!(!state.is_complete(LINK_NAVIGATION_TARGET_URL));
        state.sent_click = true;
        assert!(!state.is_complete(&state.source_url));
        assert!(!state.is_complete("about:blank#wrong-target"));
        assert!(state.is_complete(LINK_NAVIGATION_TARGET_URL));
        assert!(state
            .summary(LINK_NAVIGATION_TARGET_URL)
            .contains(LINK_NAVIGATION_TARGET_MARKER));
    }

    #[test]
    fn args_default_to_the_input_probe_page_when_probe_mode_has_no_url() {
        let args = VerifyArgs::parse(
            vec!["/usr/bin/mde-web-preview".to_owned()],
            VerifyMode::Input,
        );

        assert_eq!(args.helper, "/usr/bin/mde-web-preview");
        assert!(args.url.starts_with("data:text/html;charset=utf-8,"));
        assert!(args.url.contains("mde-browser-verify-p0-k0-t_"));
        assert_eq!(args.secs, 20);
    }

    #[test]
    fn args_treat_blank_url_as_missing_without_losing_timeout() {
        let args = VerifyArgs::parse(
            vec![
                "/usr/bin/mde-web-cef".to_owned(),
                " ".to_owned(),
                "30".to_owned(),
            ],
            VerifyMode::Input,
        );

        assert_eq!(args.helper, "/usr/bin/mde-web-cef");
        assert!(args.url.starts_with("data:text/html;charset=utf-8,"));
        assert_eq!(args.secs, 30);
    }

    #[test]
    fn args_keep_explicit_url_and_timeout() {
        let args = VerifyArgs::parse(
            vec![
                "/usr/bin/mde-web-cef".to_owned(),
                "https://example.com/".to_owned(),
                "7".to_owned(),
            ],
            VerifyMode::Display,
        );

        assert_eq!(args.helper, "/usr/bin/mde-web-cef");
        assert_eq!(args.url, "https://example.com/");
        assert_eq!(args.secs, 7);
    }

    #[test]
    fn idle_media_probe_url_contains_muted_video_canvas_stream_markers() {
        let url = idle_media_probe_url();
        assert!(url.starts_with("data:text/html;charset=utf-8,"));
        assert!(url.contains("mde-browser-idle-media-f0"));
        assert!(url.contains("c.captureStream"));
        assert!(url.contains("%3Cvideo%20id%3Dv%20autoplay%20muted%20playsinline"));
        assert!(url.contains("requestAnimationFrame"));
    }

    #[test]
    fn args_default_to_idle_media_probe_and_long_budget_in_idle_mode() {
        let args = VerifyArgs::parse(
            vec!["/usr/bin/mde-web-cef".to_owned()],
            VerifyMode::IdleMedia,
        );

        assert_eq!(args.helper, "/usr/bin/mde-web-cef");
        assert!(args.url.starts_with("data:text/html;charset=utf-8,"));
        assert!(args.url.contains("mde-browser-idle-media-f0"));
        assert_eq!(args.secs, 70);
    }

    #[test]
    fn media_metadata_playing_state_matches_cef_pump_contract() {
        assert!(media_metadata_reports_playing(
            r#"{"title":"MDE idle media","paused":false,"position_ms":1200}"#
        ));
        assert!(media_metadata_reports_playing(r#"{"paused" : false}"#));
        assert!(!media_metadata_reports_playing(r#"{"paused":true}"#));
        assert!(!media_metadata_reports_playing(
            r#"{"title":"MDE idle media"}"#
        ));
    }

    #[test]
    fn animated_frame_signature_changes_with_sampled_pixels() {
        let mut first = ColorImage::new([4, 4], Color32::from_rgb(10, 20, 30));
        let mut second = first.clone();
        second.pixels[10] = Color32::from_rgb(200, 40, 90);

        assert_ne!(
            animated_frame_signature(&first),
            animated_frame_signature(&second)
        );

        first.pixels[10] = Color32::from_rgb(200, 40, 90);
        assert_eq!(
            animated_frame_signature(&first),
            animated_frame_signature(&second)
        );
    }

    #[test]
    fn frame_signature_sampling_covers_edges_and_center() {
        assert_eq!(sample_positions(1), [0, 0, 0, 0, 0]);
        assert_eq!(sample_positions(8), [0, 2, 4, 6, 7]);
    }
}

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
//! (Servo) and can prove mouse+keyboard response with `MDE_BROWSER_VERIFY_INPUT=1`.
//!
//! Usage: `cef-verify <helper_bin> <url> [seconds]`
//!   e.g. `cef-verify /usr/bin/mde-web-cef https://example.com/ 20`
//!   e.g. `MDE_BROWSER_VERIFY_INPUT=1 cef-verify /usr/bin/mde-web-preview`

use std::io::Write as _;
use std::time::{Duration, Instant};

use mde_web_preview_client::egui::{self, pos2};
use mde_web_preview_client::session::{SpawnSpec, WebSession};

fn main() {
    let mut args = std::env::args().skip(1);
    let helper = args
        .next()
        .unwrap_or_else(|| "/usr/bin/mde-web-cef".to_string());
    let input_probe = std::env::var_os("MDE_CEF_VERIFY_INPUT").is_some()
        || std::env::var_os("MDE_BROWSER_VERIFY_INPUT").is_some();
    let url = args.next().unwrap_or_else(|| {
        if input_probe {
            input_probe_url()
        } else {
            "about:blank".to_string()
        }
    });
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let spec = SpawnSpec {
        helper_bin: helper.clone().into(),
        url: url.clone(),
        width: 1280,
        height: 800,
    };
    println!("VERIFY spawn helper={helper} url={url} budget={secs}s");
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
    // Servo's minimal helper currently does not publish dynamic title changes,
    // so page-text polling is the cross-engine observable for input response.
    let page_text_input_probe =
        input_probe || std::env::var_os("MDE_BROWSER_VERIFY_PAGE_TEXT_INPUT").is_some();
    let mut input_probe_state = InputProbeState::new(page_text_input_probe);
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        sess.poll();
        if input_probe {
            input_probe_state.drain_page_text(&mut sess);
            input_probe_state.maybe_request_page_text(&mut sess);
        }
        if let Some(frame) = sess.take_frame() {
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
        if input_probe {
            drive_input_probe(&mut sess, frame_events, &mut input_probe_state);
        }
        if input_probe
            && nav_events > 0
            && frame_events > 0
            && input_probe_state.is_complete(sess.title())
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
    if nav_events > 0 && frame_events > 0 && input_ok {
        if input_probe {
            println!("VERIFY RESULT=PASS display/load/input response observed over the wire");
        } else {
            println!(
                "VERIFY RESULT=PASS display/load handler fired and a frame arrived over the wire"
            );
        }
    } else {
        println!("VERIFY RESULT=FAIL missing NavState, frame, or input response over the wire");
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }
    let _ = std::io::stdout().flush();
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

#[cfg(test)]
mod tests {
    use super::{data_url, input_probe_url};

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
}

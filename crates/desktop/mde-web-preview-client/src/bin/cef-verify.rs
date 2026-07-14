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
//!
//! This is the honest end-to-end proof that the CEF display + load handler blocks
//! are dispatched by the real CEF vtable under real navigation — captured through
//! the same AF_UNIX wire the shell consumes, with NO shell and NO reboot. The
//! callbacks fire inside the OS-sandboxed CEF host (no writable host FS), so the
//! wire is the only observable channel — which is exactly what this reads.
//!
//! Usage: `cef-verify <helper_bin> <url> [seconds]`
//!   e.g. `cef-verify /usr/bin/mde-web-cef https://example.com/ 20`

use std::time::{Duration, Instant};

use mde_web_preview_client::session::{SpawnSpec, WebSession};

fn main() {
    let mut args = std::env::args().skip(1);
    let helper = args
        .next()
        .unwrap_or_else(|| "/usr/bin/mde-web-cef".to_string());
    let url = args.next().unwrap_or_else(|| "about:blank".to_string());
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
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        sess.poll();
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
        std::thread::sleep(Duration::from_millis(50));
    }

    println!(
        "VERIFY DONE nav_events={nav_events} title_events={title_events} favicon={favicon_seen} final_url={} final_title={}",
        sess.nav().url,
        sess.title(),
    );
    if nav_events > 0 {
        println!("VERIFY RESULT=PASS display/load handler fired: NavState delivered over the wire");
    } else {
        println!("VERIFY RESULT=FAIL no NavState received (callback did not reach the wire)");
    }
}

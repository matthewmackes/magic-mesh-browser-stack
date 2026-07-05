//! Headless Definition-of-Done tests (BOOKMARKS-5):
//! **about:blank -> a frame arrives on the shm channel**, and the
//! BUG-BROWSER-6 regression: **page content actually composites into the
//! read-back frame** (a uniform white/black capture can never pass green).
//!
//! Drives the real binary end-to-end (boot Servo -> load about:blank -> paint ->
//! read back -> publish to the shm channel), the closest thing to how the shell
//! spawns it. Running the engine in its own process (rather than a libtest
//! thread) keeps Servo on a process main thread, which it expects. Software
//! rendering is forced so the frame is produced on a GPU-less build VM.

use std::process::Command;

#[test]
fn about_blank_produces_a_frame_on_the_shm_channel() {
    let bin = env!("CARGO_BIN_EXE_mde-web-preview");
    let output = Command::new(bin)
        .args([
            "render-once",
            "--url",
            "about:blank",
            "--width",
            "320",
            "--height",
            "240",
        ])
        // Force the mesa software rasterizer so a frame is produced without a GPU.
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("GALLIUM_DRIVER", "llvmpipe")
        .output()
        .expect("spawn mde-web-preview");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "render-once exited non-zero.\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("FRAME_OK 320x240"),
        "no frame reached the shm channel.\nstdout={stdout}\nstderr={stderr}"
    );
    // A real, non-empty frame with a bumped seqlock sequence.
    assert!(
        stdout.contains("seq="),
        "no shm sequence reported.\nstdout={stdout}"
    );
    assert!(
        !stdout.contains("bytes=0"),
        "the frame was empty.\nstdout={stdout}"
    );
    // The render aid: distinct-byte + mean-luma stats ride the FRAME_OK line.
    assert!(
        stdout.contains("distinct=") && stdout.contains("mean_luma="),
        "the render-once content stats are missing.\nstdout={stdout}"
    );

    // force_emit acceptance: the watchdog path publishes a frame with NO fresh
    // frame-ready, advancing the shm sequence past the first paint. This is the
    // same code path the `tab` serve loop's first-frame watchdog uses to guarantee
    // a slow/heavy page never leaves the shell stuck on "Loading the page…".
    let frame_seq = seq_after(&stdout, "FRAME_OK").expect("a FRAME_OK seq");
    let force_seq = seq_after(&stdout, "FORCE_OK");
    assert!(
        force_seq.is_some(),
        "force_emit did not publish a FORCE_OK line.\nstdout={stdout}"
    );
    let force_seq = force_seq.expect("asserted Some above");
    assert!(
        force_seq > frame_seq,
        "force_emit did not advance the sequence: FRAME_OK seq={frame_seq}, FORCE_OK seq={force_seq}\nstdout={stdout}"
    );
    assert_eq!(
        force_seq % 2,
        0,
        "a published (stable) sequence must be even"
    );
}

/// BUG-BROWSER-6 regression: page CONTENT must composite into the read-back
/// frame. A black-background page must never read back as a UNIFORM frame —
/// uniform white (`distinct=1`, `mean_luma=255`) was the first-frame capture
/// bug (`render-once` returned on the frame WebRender generated for the
/// initial EMPTY scene, before the page's display list existed), and the
/// black mirror of it was the read-after-present bug. Asserting non-uniform +
/// dark kills both classes: neither an all-white nor an all-black frame can
/// pass green again.
#[test]
fn page_content_composites_into_the_read_back_frame() {
    let bin = env!("CARGO_BIN_EXE_mde-web-preview");
    let output = Command::new(bin)
        .args([
            "render-once",
            "--url",
            // Instant (no network), black background, white heading: even with
            // no fonts on a headless builder the black body alone must appear.
            // NB: the color hashes MUST be percent-encoded — a raw `#` starts
            // the URL FRAGMENT and silently truncates the document to
            // `<body style=background:` (an empty white page), which burned
            // the live BUG-BROWSER-6 debugging as a false repro.
            "data:text/html,<body style=background:%23000><h1 style=color:%23fff>HELLO</h1></body>",
            "--width",
            "320",
            "--height",
            "240",
        ])
        // Force the mesa software rasterizer so a frame is produced without a GPU.
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("GALLIUM_DRIVER", "llvmpipe")
        .output()
        .expect("spawn mde-web-preview");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "render-once exited non-zero.\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("FRAME_OK 320x240"),
        "no frame reached the shm channel.\nstdout={stdout}\nstderr={stderr}"
    );

    let distinct = stat_after(&stdout, "distinct=").expect("a distinct= stat on FRAME_OK");
    let mean_luma = stat_after(&stdout, "mean_luma=").expect("a mean_luma= stat on FRAME_OK");
    assert!(
        distinct > 1.0,
        "UNIFORM frame (distinct={distinct}): the page content never composited into \
         the read-back surface (BUG-BROWSER-6).\nstdout={stdout}"
    );
    assert!(
        mean_luma < 128.0,
        "a black-background page read back bright (mean_luma={mean_luma}): that is the \
         shell-background clear, not the page.\nstdout={stdout}"
    );
}

/// Parse the numeric value following `key` (e.g. `distinct=`) in `stdout`.
fn stat_after(stdout: &str, key: &str) -> Option<f64> {
    let rest = stdout.split(key).nth(1)?;
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse().ok()
}

/// Parse the `seq=<n>` value out of the line beginning with `tag` in `stdout`.
fn seq_after(stdout: &str, tag: &str) -> Option<u64> {
    let line = stdout.lines().find(|l| l.trim_start().starts_with(tag))?;
    let rest = line.split("seq=").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

//! Headless Definition-of-Done test (BOOKMARKS-5):
//! **about:blank -> a frame arrives on the shm channel.**
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

/// Parse the `seq=<n>` value out of the line beginning with `tag` in `stdout`.
fn seq_after(stdout: &str, tag: &str) -> Option<u64> {
    let line = stdout.lines().find(|l| l.trim_start().starts_with(tag))?;
    let rest = line.split("seq=").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

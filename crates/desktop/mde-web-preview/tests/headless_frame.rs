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
}

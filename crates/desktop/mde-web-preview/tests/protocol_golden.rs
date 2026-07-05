//! Golden byte-compat for the BOOKMARKS-6 socket seam.
//!
//! The helper's `EventMsg` encoding MUST be byte-identical to the client's
//! (`mde-web-preview-client`) — the two ends of one socket. The `wire` module is
//! literally shared (`#[path]`-included), so this is identity by construction, but
//! pinning the exact bytes here (mirrored in the client's `wire` tests) turns any
//! accidental un-share-and-drift into a red golden rather than a silent break: a
//! stuck-on-"Loading the page…" regression is exactly what a drift here would cause.

use mde_web_preview::wire::{take_frame, EventMsg};

#[test]
fn attach_and_paint_ready_encode_to_the_pinned_golden() {
    // AttachFrame: tag 0, no fields.
    assert_eq!(EventMsg::AttachFrame.encode(), vec![0u8]);
    // PaintReady: tag 1 + u64 LE seq.
    assert_eq!(
        EventMsg::PaintReady {
            seq: 0x0102_0304_0506_0708
        }
        .encode(),
        vec![1, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
    );
}

#[test]
fn a_framed_paint_ready_round_trips_through_the_length_prefix() {
    // The bytes that actually cross the socket: [u32 LE len][payload].
    let payload = EventMsg::PaintReady { seq: 7 }.encode();
    let mut wire_bytes = mde_web_preview::wire::frame(&payload);
    assert_eq!(
        &wire_bytes[..4],
        &9u32.to_le_bytes(),
        "len prefix = 9 bytes"
    );
    let popped = take_frame(&mut wire_bytes)
        .expect("no framing error")
        .expect("one full frame");
    assert_eq!(
        EventMsg::decode(&popped).expect("decode"),
        EventMsg::PaintReady { seq: 7 }
    );
}

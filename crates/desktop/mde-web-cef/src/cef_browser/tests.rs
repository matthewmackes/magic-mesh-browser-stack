//! Unit tests for the CEF browser bridge (relocated verbatim from mod.rs).
use super::*;
use std::mem::{align_of, size_of};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn cursor_change_offset_and_type_mapping() {
    // on_cursor_change is field 9 of cef_display_handler_t; on_title_change=48
    // pins field 1, and the 144-byte / 13-field handler bounds it.
    assert_eq!(CEF_DISPLAY_HANDLER_ON_CURSOR_CHANGE_OFFSET, 40 + 9 * 8);
    assert_eq!(CEF_DISPLAY_HANDLER_ON_TITLE_CHANGE_OFFSET, 40 + 1 * 8);
    // The load-bearing CT_* → CursorKind mappings (verified against the CEF 149
    // cef_cursor_type_t enum).
    assert_eq!(cursor_kind_for_cef_type(0), CursorKind::Default); // CT_POINTER
    assert_eq!(cursor_kind_for_cef_type(2), CursorKind::Pointer); // CT_HAND
    assert_eq!(cursor_kind_for_cef_type(3), CursorKind::Text); // CT_IBEAM
    assert_eq!(cursor_kind_for_cef_type(6), CursorKind::ResizeHorizontal); // CT_EASTRESIZE
    assert_eq!(cursor_kind_for_cef_type(7), CursorKind::ResizeVertical); // CT_NORTHRESIZE
    assert_eq!(cursor_kind_for_cef_type(41), CursorKind::Grab); // CT_GRAB
    assert_eq!(cursor_kind_for_cef_type(42), CursorKind::Grabbing); // CT_GRABBING
    assert_eq!(cursor_kind_for_cef_type(38), CursorKind::NotAllowed); // CT_NOTALLOWED
    assert_eq!(cursor_kind_for_cef_type(999), CursorKind::Default); // unmapped
}

/// A `w × h` BGRA buffer filled with `value` in every byte.
fn bgra(w: i64, h: i64, value: u8) -> Vec<u8> {
    vec![value; usize::try_from(w * h * 4).expect("test dims")]
}

#[test]
fn popup_blend_copies_the_rect_and_clips_at_every_edge() {
    // A 2x2 popup at (1, 1) inside a 4x4 view: exactly those 4 pixels change.
    let mut view = bgra(4, 4, 0x00);
    blend_popup_over_view(&mut view, 4, 4, &bgra(2, 2, 0xFF), 2, 2, 1, 1);
    let px = |x: usize, y: usize| view[(y * 4 + x) * 4];
    assert_eq!(px(0, 0), 0x00);
    assert_eq!(px(1, 1), 0xFF);
    assert_eq!(px(2, 2), 0xFF);
    assert_eq!(px(3, 3), 0x00);
    assert_eq!(px(1, 0), 0x00, "row above the popup untouched");

    // Negative origin clips top/left instead of panicking or wrapping.
    let mut view = bgra(4, 4, 0x00);
    blend_popup_over_view(&mut view, 4, 4, &bgra(2, 2, 0xFF), 2, 2, -1, -1);
    let px = |x: usize, y: usize| view[(y * 4 + x) * 4];
    assert_eq!(px(0, 0), 0xFF, "the surviving popup corner lands at 0,0");
    assert_eq!(px(1, 1), 0x00);

    // Overhanging the bottom-right edge clips there too.
    let mut view = bgra(4, 4, 0x00);
    blend_popup_over_view(&mut view, 4, 4, &bgra(3, 3, 0xFF), 3, 3, 3, 3);
    let px = |x: usize, y: usize| view[(y * 4 + x) * 4];
    assert_eq!(px(3, 3), 0xFF);
    assert_eq!(px(2, 2), 0x00);

    // A popup entirely off-view is a no-op.
    let mut view = bgra(2, 2, 0x11);
    blend_popup_over_view(&mut view, 2, 2, &bgra(2, 2, 0xFF), 2, 2, 10, 10);
    assert!(view.iter().all(|&b| b == 0x11));
}

#[test]
fn popup_compose_needs_both_a_view_and_popup_pixels() {
    // No retained view → nothing to composite (the PET_POPUP paint waits for
    // the next view frame).
    let mut overlay = PopupOverlay {
        visible: true,
        rect: (1, 1, 2, 2),
        pixels: Some(bgra(2, 2, 0xFF)),
        view: None,
    };
    assert!(overlay.compose().is_none());
    // With a view retained, the composite is the view with the rect blended.
    overlay.view = Some((4, 4, bgra(4, 4, 0x00)));
    let (w, h, merged) = overlay.compose().expect("composite");
    assert_eq!((w, h), (4, 4));
    assert_eq!(merged[(4 + 1) * 4], 0xFF, "popup pixel at (1,1)");
    assert_eq!(merged[0], 0x00, "view pixel at (0,0) untouched");
}

#[test]
fn before_popup_offset_reconciles_with_the_life_span_layout() {
    // cef_life_span_handler_t (CEF 149): on_before_popup is field 0; the proven
    // on_after_created=64 pins field 3, on_before_close is field 5, and the
    // handler size 88 pins 6 fields.
    assert_eq!(CEF_LIFE_SPAN_ON_BEFORE_POPUP_OFFSET, 40);
    assert_eq!(CEF_LIFE_SPAN_ON_AFTER_CREATED_OFFSET, 40 + 3 * 8);
    assert_eq!(CEF_LIFE_SPAN_ON_BEFORE_CLOSE_OFFSET, 40 + 5 * 8);
    assert_eq!(CEF_LIFE_SPAN_HANDLER_SIZE, 40 + 6 * 8);
}

#[test]
fn render_process_terminated_offset_reconciles_with_the_request_handler_layout() {
    // cef_request_handler_t (CEF 149) is 11 fn ptrs after the 40-byte base; the
    // proven get_resource_request_handler=56 pins index 2, and the struct size
    // 128 = 40 + 11*8 pins the field count. on_render_process_terminated is
    // index 9 (before on_document_available_in_main_frame at index 10).
    assert_eq!(
        CEF_REQUEST_HANDLER_ON_RENDER_PROCESS_TERMINATED_OFFSET,
        40 + 9 * 8
    );
    assert_eq!(CEF_REQUEST_HANDLER_SIZE, 40 + 11 * 8);
    assert!(
        CEF_REQUEST_HANDLER_ON_RENDER_PROCESS_TERMINATED_OFFSET < CEF_REQUEST_HANDLER_SIZE - 8,
        "terminated slot must leave room for the final field"
    );
}

#[test]
fn certificate_error_offset_sits_inside_the_request_handler_layout() {
    // cef_request_handler_t (CEF 149): the proven get_resource_request_handler=56
    // pins index 2, so on_certificate_error is index 4 (get_auth_credentials=64,
    // on_certificate_error=72), before the proven on_render_process_terminated=112.
    assert_eq!(CEF_REQUEST_HANDLER_ON_CERTIFICATE_ERROR_OFFSET, 40 + 4 * 8);
    assert!(
        CEF_REQUEST_HANDLER_GET_RESOURCE_REQUEST_HANDLER_OFFSET
            < CEF_REQUEST_HANDLER_ON_CERTIFICATE_ERROR_OFFSET
    );
    assert!(
        CEF_REQUEST_HANDLER_ON_CERTIFICATE_ERROR_OFFSET
            < CEF_REQUEST_HANDLER_ON_RENDER_PROCESS_TERMINATED_OFFSET
    );
    assert!(CEF_REQUEST_HANDLER_ON_CERTIFICATE_ERROR_OFFSET < CEF_REQUEST_HANDLER_SIZE - 8);
}

#[test]
fn get_resource_type_offset_sits_inside_the_request_layout() {
    // cef_request_t (pinned CEF 149, cross-checked on-seat .15): get_resource_type
    // is index 19 of the fn-ptr block that follows the 40-byte base, so
    // 40 + 19*8 = 192. Anchored non-tautologically against the two struct-pinning
    // constants: get_url=48 fixes index 1 (40 + 1*8), and CEF_REQUEST_SIZE=216 =
    // 40 + 22*8 fixes the method count at 22 (indices 0..21), so index 19 is the
    // third-from-last fn ptr and must sit inside the struct.
    assert_eq!(CEF_REQUEST_GET_RESOURCE_TYPE_OFFSET, 40 + 19 * 8);
    assert_eq!(CEF_REQUEST_GET_URL_OFFSET, 40 + 1 * 8);
    assert_eq!(CEF_REQUEST_SIZE, 40 + 22 * 8);
    assert!(
        CEF_REQUEST_SET_HEADER_BY_NAME_OFFSET < CEF_REQUEST_GET_RESOURCE_TYPE_OFFSET,
        "set_header_by_name (idx 13) precedes get_resource_type (idx 19)"
    );
    assert!(
        CEF_REQUEST_GET_RESOURCE_TYPE_OFFSET < CEF_REQUEST_SIZE - 8,
        "get_resource_type fn ptr lies wholly inside cef_request_t"
    );
}

#[test]
fn cef_resource_type_maps_to_the_shell_wire_bytes() {
    // CEF cef_resource_type_t (on-seat) -> the compact byte resource_from_wire
    // decodes in mde-web-preview-client filter.rs. The remap is deliberate: CEF's
    // MEDIA=8/XHR=13/PING=14 are NOT the shell ResourceType discriminants, so a
    // cast would mis-classify them.
    assert_eq!(cef_resource_type_to_wire(0), 0, "MAIN_FRAME -> Document");
    assert_eq!(cef_resource_type_to_wire(1), 1, "SUB_FRAME -> Subdocument");
    assert_eq!(cef_resource_type_to_wire(2), 2, "STYLESHEET");
    assert_eq!(cef_resource_type_to_wire(3), 3, "SCRIPT");
    assert_eq!(cef_resource_type_to_wire(4), 4, "IMAGE");
    assert_eq!(cef_resource_type_to_wire(5), 5, "FONT_RESOURCE -> Font");
    assert_eq!(cef_resource_type_to_wire(8), 6, "MEDIA -> Media");
    assert_eq!(cef_resource_type_to_wire(7), 7, "OBJECT");
    assert_eq!(cef_resource_type_to_wire(13), 8, "XHR -> XmlHttpRequest");
    assert_eq!(cef_resource_type_to_wire(14), 9, "PING -> Ping");
    // The null-vtable sentinel and any unmapped/plugin class fall back to Other,
    // NOT to the MAIN_FRAME (0) a naive cast of -1 or 6 would risk.
    assert_eq!(
        cef_resource_type_to_wire(-1),
        RESOURCE_OTHER,
        "null sentinel"
    );
    assert_eq!(cef_resource_type_to_wire(6), RESOURCE_OTHER, "SUB_RESOURCE");
    assert_eq!(
        cef_resource_type_to_wire(99),
        RESOURCE_OTHER,
        "future class"
    );
}

#[test]
fn jsdialog_offsets_reconcile_with_the_pinned_cef_layout() {
    // cef_client_t: get_jsdialog_handler is index 11 (base 40 + 11*8 = 128),
    // sitting between get_permission_handler(120) and get_keyboard_handler(136),
    // and inside the proven CEF_CLIENT_SIZE=192.
    assert_eq!(CEF_CLIENT_GET_JSDIALOG_HANDLER_OFFSET, 40 + 11 * 8);
    assert!(CEF_CLIENT_GET_JSDIALOG_HANDLER_OFFSET < CEF_CLIENT_SIZE - 8);
    // cef_jsdialog_handler_t: on_jsdialog is index 0 (base 40),
    // on_before_unload_dialog is index 1 (48), and the struct is 4 methods
    // (72 bytes); both registered slots fit inside it.
    assert_eq!(CEF_JSDIALOG_HANDLER_ON_JSDIALOG_OFFSET, 40);
    assert_eq!(CEF_JSDIALOG_HANDLER_ON_BEFORE_UNLOAD_DIALOG_OFFSET, 40 + 8);
    assert_eq!(CEF_JSDIALOG_HANDLER_SIZE, 40 + 4 * 8);
    assert!(CEF_JSDIALOG_HANDLER_ON_JSDIALOG_OFFSET < CEF_JSDIALOG_HANDLER_SIZE);
    assert!(CEF_JSDIALOG_HANDLER_ON_BEFORE_UNLOAD_DIALOG_OFFSET < CEF_JSDIALOG_HANDLER_SIZE);
    // The jsdialog callback's cont slot reuses the shared cont offset (40).
    assert_eq!(CEF_CALLBACK_CONT_OFFSET, 40);
}

#[test]
fn popup_render_handler_offsets_sit_between_view_rect_and_paint() {
    // cef_render_handler_t field order pins on_popup_show/on_popup_size between
    // the two offsets this bridge has already proven live (get_view_rect=56,
    // on_paint=96): screen_point(64), screen_info(72), popup_show(80), popup_size(88).
    assert_eq!(CEF_RENDER_HANDLER_ON_POPUP_SHOW_OFFSET, 80);
    assert_eq!(CEF_RENDER_HANDLER_ON_POPUP_SIZE_OFFSET, 88);
    assert!(CEF_RENDER_HANDLER_GET_VIEW_RECT_OFFSET < CEF_RENDER_HANDLER_ON_POPUP_SHOW_OFFSET);
    assert!(CEF_RENDER_HANDLER_ON_POPUP_SIZE_OFFSET < CEF_RENDER_HANDLER_ON_PAINT_OFFSET);
}

#[test]
fn multi_click_chains_within_the_window_and_radius_and_caps_at_triple() {
    let base = std::time::Instant::now();
    let mut tracker = ClickTracker::new();
    assert_eq!(tracker.register(base, 100, 100, PointerButton::Primary), 1);
    // A second press 200 ms later, 2 px away → a double-click.
    let second = base + std::time::Duration::from_millis(200);
    assert_eq!(
        tracker.register(second, 102, 101, PointerButton::Primary),
        2
    );
    // The matching release carries the press's count.
    assert_eq!(tracker.release_count(PointerButton::Primary), 2);
    // Third chained press → triple (paragraph select)…
    let third = second + std::time::Duration::from_millis(200);
    assert_eq!(tracker.register(third, 100, 100, PointerButton::Primary), 3);
    // …and further rapid presses hold at 3 rather than cycling to a caret.
    let fourth = third + std::time::Duration::from_millis(200);
    assert_eq!(
        tracker.register(fourth, 100, 100, PointerButton::Primary),
        3
    );
}

#[test]
fn multi_click_resets_when_slow_far_or_another_button() {
    let base = std::time::Instant::now();
    let mut tracker = ClickTracker::new();
    assert_eq!(tracker.register(base, 100, 100, PointerButton::Primary), 1);
    // Slower than the double-click window → back to a single click.
    let slow = base + std::time::Duration::from_millis(800);
    assert_eq!(tracker.register(slow, 100, 100, PointerButton::Primary), 1);
    // Farther than the chaining radius → single.
    let far = slow + std::time::Duration::from_millis(100);
    assert_eq!(tracker.register(far, 160, 100, PointerButton::Primary), 1);
    // A different button never chains, and the primary's release count resets.
    let other = far + std::time::Duration::from_millis(100);
    assert_eq!(
        tracker.register(other, 160, 100, PointerButton::Secondary),
        1
    );
    assert_eq!(tracker.release_count(PointerButton::Primary), 1);
}

#[test]
fn held_buttons_or_into_drag_move_flags_and_clear_on_release() {
    // Pure flag math (the state machine drives real CEF mouse-moves): each
    // button maps to its own EVENTFLAG bit so a drag-move carries exactly the
    // held set.
    let held = mouse_button_event_flag(PointerButton::Primary)
        | mouse_button_event_flag(PointerButton::Middle);
    assert_eq!(
        held & EVENTFLAG_LEFT_MOUSE_BUTTON,
        EVENTFLAG_LEFT_MOUSE_BUTTON
    );
    assert_eq!(
        held & EVENTFLAG_MIDDLE_MOUSE_BUTTON,
        EVENTFLAG_MIDDLE_MOUSE_BUTTON
    );
    assert_eq!(held & EVENTFLAG_RIGHT_MOUSE_BUTTON, 0);
    let after_release = held & !mouse_button_event_flag(PointerButton::Primary);
    assert_eq!(after_release, EVENTFLAG_MIDDLE_MOUSE_BUTTON);
}

#[test]
fn lookup_peer_handler_sizes_are_unique() {
    // `lookup_peer` resolves a handler block for a state by its byte size, so every
    // handler resolved that way MUST have a distinct size — a collision would hand
    // CEF the wrong vtable. (life_span and print both == 88, which is exactly why
    // print is resolved via an explicit stored ptr, not lookup_peer.) B1 added the
    // display + load handlers; guard that they don't collide with the existing set.
    let sizes = [
        ("life_span", CEF_LIFE_SPAN_HANDLER_SIZE),
        ("render", CEF_RENDER_HANDLER_SIZE),
        ("request", CEF_REQUEST_HANDLER_SIZE),
        ("resource_request", CEF_RESOURCE_REQUEST_HANDLER_SIZE),
        ("display", CEF_DISPLAY_HANDLER_SIZE),
        ("load", CEF_LOAD_HANDLER_SIZE),
    ];
    for (i, (na, sa)) in sizes.iter().enumerate() {
        for (nb, sb) in &sizes[i + 1..] {
            assert_ne!(
                sa, sb,
                "lookup_peer handlers {na} and {nb} share size {sa} — lookup_peer would \
                 return the wrong block; give one an explicit stored ptr like print"
            );
        }
    }
}

#[test]
fn b1_client_handler_offsets_match_the_cef149_client_layout() {
    // The cef_client_t vtable is a 40-byte ref-counted base then 8-byte fn ptrs.
    // Anchor on the already-proven offsets and derive display/load by field index
    // (display = index 4, load = index 14, right before print at index 15).
    const BASE: usize = 40;
    assert_eq!(CEF_CLIENT_GET_DISPLAY_HANDLER_OFFSET, BASE + 4 * 8);
    assert_eq!(CEF_CLIENT_GET_LOAD_HANDLER_OFFSET, BASE + 14 * 8);
    // Internal consistency with the anchors this file already trusts.
    assert_eq!(
        CEF_CLIENT_GET_LOAD_HANDLER_OFFSET + 8,
        CEF_CLIENT_GET_PRINT_HANDLER_OFFSET
    );
    assert!(CEF_CLIENT_GET_DISPLAY_HANDLER_OFFSET < CEF_CLIENT_GET_LIFE_SPAN_HANDLER_OFFSET);
    // Handler struct sizes = 40-byte base + N fn ptrs.
    assert_eq!(CEF_DISPLAY_HANDLER_SIZE, BASE + 13 * 8);
    assert_eq!(CEF_LOAD_HANDLER_SIZE, BASE + 4 * 8);
    // The two display methods B1 registers sit at the front of the vtable.
    assert_eq!(CEF_DISPLAY_HANDLER_ON_ADDRESS_CHANGE_OFFSET, BASE);
    assert_eq!(CEF_DISPLAY_HANDLER_ON_TITLE_CHANGE_OFFSET, BASE + 8);
    assert_eq!(CEF_LOAD_HANDLER_ON_LOADING_STATE_CHANGE_OFFSET, BASE);
}

fn unique_test_pdf_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "mde-web-cef-{tag}-{}-{nanos}.pdf",
        std::process::id()
    ))
}

#[test]
fn callback_layout_matches_pinned_cef_headers() {
    assert_eq!(CEF_BASE_REF_COUNTED_SIZE, 40);
    assert_eq!(CEF_CLIENT_SIZE, 192);
    assert_eq!(CEF_CLIENT_GET_LIFE_SPAN_HANDLER_OFFSET, 144);
    assert_eq!(CEF_CLIENT_GET_PRINT_HANDLER_OFFSET, 160);
    assert_eq!(CEF_CLIENT_GET_RENDER_HANDLER_OFFSET, 168);
    assert_eq!(CEF_CLIENT_GET_REQUEST_HANDLER_OFFSET, 176);
    // get_audio_handler is index 0 of the client vtable — the first getter after
    // the 40-byte base. The known anchors above pin the same struct, so 40 + 0*8.
    assert_eq!(CEF_CLIENT_GET_AUDIO_HANDLER_OFFSET, 40);
    assert_eq!(CEF_CLIENT_GET_AUDIO_HANDLER_OFFSET, 40 + 0 * 8);
    // cef_audio_handler_t: 5 fn ptrs after the 40-byte base (get_audio_parameters
    // =40, on_audio_stream_started=48, packet=56, stopped=64, error=72).
    assert_eq!(CEF_AUDIO_HANDLER_SIZE, 80);
    assert_eq!(CEF_AUDIO_HANDLER_SIZE, 40 + 5 * 8);
    assert_eq!(CEF_AUDIO_HANDLER_GET_AUDIO_PARAMETERS_OFFSET, 40 + 0 * 8);
    assert_eq!(CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STARTED_OFFSET, 40 + 1 * 8);
    assert_eq!(CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_PACKET_OFFSET, 40 + 2 * 8);
    assert_eq!(CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STOPPED_OFFSET, 40 + 3 * 8);
    assert_eq!(CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_ERROR_OFFSET, 40 + 4 * 8);
    // cef_audio_parameters_t = size_t size@0 + channel_layout@8 + sample_rate@12
    // + frames_per_buffer@16 → 8 + 3*4 = 20, padded to size_t alignment → 24.
    assert_eq!(CEF_AUDIO_PARAMETERS_SIZE, 8 + 3 * 4 + 4); // 20 body + 4 tail pad
    assert_eq!(CEF_AUDIO_PARAMETERS_SIZE, 24);
    assert_eq!(size_of::<CefAudioParameters>(), CEF_AUDIO_PARAMETERS_SIZE);
    assert_eq!(CEF_LIFE_SPAN_HANDLER_SIZE, 88);
    assert_eq!(CEF_LIFE_SPAN_ON_AFTER_CREATED_OFFSET, 64);
    assert_eq!(CEF_RENDER_HANDLER_SIZE, 176);
    assert_eq!(CEF_RENDER_HANDLER_GET_VIEW_RECT_OFFSET, 56);
    assert_eq!(CEF_RENDER_HANDLER_ON_PAINT_OFFSET, 96);
    assert_eq!(CEF_REQUEST_HANDLER_SIZE, 128);
    assert_eq!(CEF_REQUEST_HANDLER_GET_RESOURCE_REQUEST_HANDLER_OFFSET, 56);
    assert_eq!(CEF_RESOURCE_REQUEST_HANDLER_SIZE, 104);
    assert_eq!(
        CEF_RESOURCE_REQUEST_HANDLER_ON_BEFORE_RESOURCE_LOAD_OFFSET,
        48
    );
    assert_eq!(CEF_PRINT_HANDLER_SIZE, 88);
    assert_eq!(CEF_PRINT_HANDLER_ON_PRINT_DIALOG_OFFSET, 56);
    assert_eq!(CEF_PRINT_HANDLER_ON_PRINT_JOB_OFFSET, 64);
    assert_eq!(CEF_PRINT_HANDLER_GET_PDF_PAPER_SIZE_OFFSET, 80);
    assert_eq!(CEF_BROWSER_SIZE, 208);
    assert_eq!(CEF_BROWSER_GET_HOST_OFFSET, 48);
    assert_eq!(CEF_BROWSER_CAN_GO_BACK_OFFSET, 56);
    assert_eq!(CEF_BROWSER_GO_BACK_OFFSET, 64);
    assert_eq!(CEF_BROWSER_CAN_GO_FORWARD_OFFSET, 72);
    assert_eq!(CEF_BROWSER_GO_FORWARD_OFFSET, 80);
    assert_eq!(CEF_BROWSER_RELOAD_OFFSET, 96);
    assert_eq!(CEF_BROWSER_STOP_LOAD_OFFSET, 112);
    assert_eq!(CEF_BROWSER_GET_MAIN_FRAME_OFFSET, 152);
    assert_eq!(CEF_BROWSER_HOST_SIZE, 592);
    assert_eq!(CEF_BROWSER_HOST_CLOSE_BROWSER_OFFSET, 48);
    assert_eq!(CEF_BROWSER_HOST_SET_FOCUS_OFFSET, 72);
    assert_eq!(CEF_BROWSER_HOST_WAS_RESIZED_OFFSET, 304);
    assert_eq!(CEF_BROWSER_HOST_INVALIDATE_OFFSET, 328);
    assert_eq!(CEF_BROWSER_HOST_SEND_KEY_EVENT_OFFSET, 344);
    assert_eq!(CEF_BROWSER_HOST_SEND_MOUSE_CLICK_EVENT_OFFSET, 352);
    assert_eq!(CEF_BROWSER_HOST_SEND_MOUSE_MOVE_EVENT_OFFSET, 360);
    assert_eq!(CEF_BROWSER_HOST_SEND_MOUSE_WHEEL_EVENT_OFFSET, 368);
    // print / print_to_pdf: fields 19/20 of the cef_browser_host_t vtable, verified via
    // `offsetof` against the pinned CEF 149 header (/opt/mde/cef/include/capi/) — a stale
    // 504/512 (fields 58/59 = set_accessibility_state / set_auto_resize_enabled) had made
    // PrintPage/SavePdf call the WRONG host methods on live CEF. Expressed as 40+idx*8 so
    // the field index is legible, not an opaque literal that can drift undetected.
    assert_eq!(CEF_BROWSER_HOST_PRINT_OFFSET, 40 + 19 * 8);
    assert_eq!(CEF_BROWSER_HOST_PRINT_OFFSET, 192);
    assert_eq!(CEF_BROWSER_HOST_PRINT_TO_PDF_OFFSET, 40 + 20 * 8);
    assert_eq!(CEF_BROWSER_HOST_PRINT_TO_PDF_OFFSET, 200);
    assert_eq!(CEF_BROWSER_HOST_SET_AUDIO_MUTED_OFFSET, 40 + 60 * 8);
    assert_eq!(CEF_BROWSER_HOST_SET_AUDIO_MUTED_OFFSET, 520);
    assert_eq!(CEF_BROWSER_HOST_IS_AUDIO_MUTED_OFFSET, 528);
    // IME slots: fields 47/48/49 of the cef_browser_host_t vtable, confirmed via
    // `offsetof` against the pinned CEF 149 header and bounded by set_audio_muted
    // (field 60 = 520). base 40 + idx*8.
    assert_eq!(CEF_BROWSER_HOST_IME_SET_COMPOSITION_OFFSET, 40 + 47 * 8);
    assert_eq!(CEF_BROWSER_HOST_IME_COMMIT_TEXT_OFFSET, 40 + 48 * 8);
    assert_eq!(CEF_BROWSER_HOST_IME_FINISH_COMPOSING_OFFSET, 40 + 49 * 8);
    assert_eq!(CEF_BROWSER_HOST_IME_SET_COMPOSITION_OFFSET, 416);
    assert_eq!(CEF_BROWSER_HOST_IME_COMMIT_TEXT_OFFSET, 424);
    assert_eq!(CEF_BROWSER_HOST_IME_FINISH_COMPOSING_OFFSET, 432);
    assert_eq!(CEF_FRAME_SIZE, 248);
    assert_eq!(CEF_FRAME_LOAD_URL_OFFSET, 144);
    assert_eq!(CEF_FRAME_EXECUTE_JAVA_SCRIPT_OFFSET, 152);
    assert_eq!(CEF_FRAME_IS_MAIN_OFFSET, 160);
    assert_eq!(CEF_REQUEST_SIZE, 216);
    assert_eq!(CEF_REQUEST_GET_URL_OFFSET, 48);
    // set_header_by_name is index 13 of the fn-ptr block after the 40-byte base;
    // the classic cef_request_capi.h core (indices 0..14) is ABI-frozen. Anchored
    // by get_url=48 (index 1) and the 22-method size 216 = 40 + 22*8.
    assert_eq!(CEF_REQUEST_SET_HEADER_BY_NAME_OFFSET, 40 + 13 * 8);
    assert_eq!(CEF_REQUEST_SET_HEADER_BY_NAME_OFFSET, 144);
    assert_eq!(CEF_CALLBACK_SIZE, 56);
    assert_eq!(CEF_CALLBACK_CONT_OFFSET, 40);
    assert_eq!(CEF_CALLBACK_CANCEL_OFFSET, 48);
    assert_eq!(CEF_PDF_PRINT_CALLBACK_SIZE, 48);
    assert_eq!(CEF_PDF_PRINT_CALLBACK_ON_FINISHED_OFFSET, 40);
    assert_eq!(CEF_STRING_VISITOR_SIZE, 48);
    assert_eq!(CEF_STRING_VISITOR_VISIT_OFFSET, 40);
    assert_eq!(CEF_MOUSE_EVENT_SIZE, 12);
    assert_eq!(CEF_MOUSE_EVENT_X_OFFSET, 0);
    assert_eq!(CEF_MOUSE_EVENT_Y_OFFSET, 4);
    assert_eq!(CEF_MOUSE_EVENT_MODIFIERS_OFFSET, 8);
    assert_eq!(CEF_KEY_EVENT_SIZE, 40);
    assert_eq!(CEF_KEY_EVENT_TYPE_OFFSET, 8);
    assert_eq!(CEF_KEY_EVENT_MODIFIERS_OFFSET, 12);
    assert_eq!(CEF_KEY_EVENT_WINDOWS_KEY_CODE_OFFSET, 16);
    assert_eq!(CEF_KEY_EVENT_NATIVE_KEY_CODE_OFFSET, 20);
    assert_eq!(CEF_KEY_EVENT_IS_SYSTEM_KEY_OFFSET, 24);
    assert_eq!(CEF_KEY_EVENT_CHARACTER_OFFSET, 28);
    assert_eq!(CEF_KEY_EVENT_UNMODIFIED_CHARACTER_OFFSET, 30);
    assert_eq!(CEF_KEY_EVENT_FOCUS_ON_EDITABLE_FIELD_OFFSET, 32);
    assert_eq!(size_of::<CefRect>(), CEF_RECT_SIZE);
    assert_eq!(size_of::<CefSize>(), 8);
    assert_eq!(align_of::<CefCallbackBlock<CEF_CLIENT_SIZE>>(), 8);
}

#[test]
fn window_info_sets_windowless_bounds_from_header_offsets() {
    let info = CefWindowInfo::windowless(800, 600);
    assert_eq!(read_usize(&info.bytes, 0), CEF_WINDOW_INFO_SIZE);
    assert_eq!(
        read_i32(&info.bytes, CEF_WINDOW_INFO_BOUNDS_OFFSET + 8),
        800
    );
    assert_eq!(
        read_i32(&info.bytes, CEF_WINDOW_INFO_BOUNDS_OFFSET + 12),
        600
    );
    assert_eq!(read_i32(&info.bytes, CEF_WINDOW_INFO_WINDOWLESS_OFFSET), 1);
    assert_eq!(
        read_i32(&info.bytes, CEF_WINDOW_INFO_SHARED_TEXTURE_OFFSET),
        0
    );
    assert_eq!(
        read_i32(&info.bytes, CEF_WINDOW_INFO_RUNTIME_STYLE_OFFSET),
        CEF_RUNTIME_STYLE_ALLOY
    );
}

#[test]
fn browser_settings_set_size_rate_and_background() {
    let settings = CefBrowserSettings::windowless(15);
    assert_eq!(read_usize(&settings.bytes, 0), CEF_BROWSER_SETTINGS_SIZE);
    assert_eq!(
        read_i32(&settings.bytes, CEF_BROWSER_SETTINGS_FRAME_RATE_OFFSET),
        15
    );
    assert_eq!(
        read_u32(
            &settings.bytes,
            CEF_BROWSER_SETTINGS_BACKGROUND_COLOR_OFFSET
        ),
        0xFFFF_FFFF
    );
}

#[test]
fn mouse_event_sets_header_pinned_fields() {
    let event = CefMouseEvent::new(12, 34, EVENTFLAG_LEFT_MOUSE_BUTTON);
    assert_eq!(read_i32(&event.bytes, CEF_MOUSE_EVENT_X_OFFSET), 12);
    assert_eq!(read_i32(&event.bytes, CEF_MOUSE_EVENT_Y_OFFSET), 34);
    assert_eq!(
        read_i32(&event.bytes, CEF_MOUSE_EVENT_MODIFIERS_OFFSET),
        EVENTFLAG_LEFT_MOUSE_BUTTON
    );
}

#[test]
fn key_event_sets_header_pinned_fields() {
    let event = CefKeyEvent::new(
        KEYEVENT_RAWKEYDOWN,
        EVENTFLAG_SHIFT_DOWN,
        65,
        65,
        b'A' as u16,
        b'A' as u16,
    );
    assert_eq!(read_usize(&event.bytes, 0), CEF_KEY_EVENT_SIZE);
    assert_eq!(
        read_i32(&event.bytes, CEF_KEY_EVENT_TYPE_OFFSET),
        KEYEVENT_RAWKEYDOWN
    );
    assert_eq!(
        read_i32(&event.bytes, CEF_KEY_EVENT_MODIFIERS_OFFSET),
        EVENTFLAG_SHIFT_DOWN
    );
    assert_eq!(
        read_i32(&event.bytes, CEF_KEY_EVENT_WINDOWS_KEY_CODE_OFFSET),
        65
    );
    assert_eq!(
        read_i32(&event.bytes, CEF_KEY_EVENT_NATIVE_KEY_CODE_OFFSET),
        65
    );
    assert_eq!(
        read_u16(&event.bytes, CEF_KEY_EVENT_CHARACTER_OFFSET),
        b'A' as u16
    );
    assert_eq!(
        read_u16(&event.bytes, CEF_KEY_EVENT_UNMODIFIED_CHARACTER_OFFSET),
        b'A' as u16
    );
    assert_eq!(
        read_i32(&event.bytes, CEF_KEY_EVENT_FOCUS_ON_EDITABLE_FIELD_OFFSET),
        1
    );
}

static FOCUS_CALLS: AtomicI32 = AtomicI32::new(0);
static FOCUS_LAST: AtomicI32 = AtomicI32::new(0);
static KEY_EVENT_CALLS: AtomicI32 = AtomicI32::new(0);
static KEY_EVENT_LAST_TYPE: AtomicI32 = AtomicI32::new(-1);
static KEY_EVENT_LAST_WINDOWS_CODE: AtomicI32 = AtomicI32::new(-1);
static KEY_EVENT_LAST_CHAR: AtomicI32 = AtomicI32::new(-1);
static STOP_LOAD_CALLS: AtomicI32 = AtomicI32::new(0);
static AUDIO_MUTED_CALLS: AtomicI32 = AtomicI32::new(0);
static AUDIO_MUTED_LAST: AtomicI32 = AtomicI32::new(0);
static TEST_HOST_PTR: AtomicUsize = AtomicUsize::new(0);
static TEST_MAIN_FRAME_PTR: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn test_browser_host(_browser: *mut c_void) -> *mut c_void {
    TEST_HOST_PTR.load(AtomicOrdering::SeqCst) as *mut c_void
}

unsafe extern "C" fn test_main_frame(_browser: *mut c_void) -> *mut c_void {
    TEST_MAIN_FRAME_PTR.load(AtomicOrdering::SeqCst) as *mut c_void
}

unsafe extern "C" fn test_frame_is_main(_frame: *mut c_void) -> c_int {
    1
}

unsafe extern "C" fn test_frame_is_subframe(_frame: *mut c_void) -> c_int {
    0
}

unsafe extern "C" fn record_focus(_host: *mut c_void, focused: c_int) {
    FOCUS_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    FOCUS_LAST.store(focused, AtomicOrdering::SeqCst);
}

#[test]
fn cef_address_changes_only_update_top_level_url_from_the_main_frame() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    browser
        [CEF_BROWSER_GET_MAIN_FRAME_OFFSET..CEF_BROWSER_GET_MAIN_FRAME_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(test_main_frame as *const () as usize).to_ne_bytes());
    let mut main_wrapper = vec![0u8; CEF_FRAME_SIZE];
    let mut main = vec![0u8; CEF_FRAME_SIZE];
    let mut subframe = vec![0u8; CEF_FRAME_SIZE];
    main[CEF_FRAME_IS_MAIN_OFFSET..CEF_FRAME_IS_MAIN_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(test_frame_is_main as *const () as usize).to_ne_bytes());
    subframe[CEF_FRAME_IS_MAIN_OFFSET..CEF_FRAME_IS_MAIN_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(test_frame_is_subframe as *const () as usize).to_ne_bytes());
    TEST_MAIN_FRAME_PTR.store(main_wrapper.as_mut_ptr() as usize, AtomicOrdering::SeqCst);

    let display = callbacks.state.display_ptr();
    let browser = browser.as_mut_ptr().cast();
    let main_url = CefStringOwned::new("https://news.example/article").expect("main url");
    let subframe_url = CefStringOwned::new("https://ads.example/frame").expect("subframe url");

    unsafe {
        on_address_change(
            display,
            browser,
            subframe.as_mut_ptr().cast(),
            subframe_url.as_ptr().cast::<CefString>(),
        );
    }
    assert_eq!(
        callbacks.state.current_top_level_url(),
        "",
        "a subframe address event must not claim top-level browser chrome"
    );

    unsafe {
        on_address_change(
            display,
            browser,
            main.as_mut_ptr().cast(),
            main_url.as_ptr().cast::<CefString>(),
        );
    }
    assert_eq!(
        callbacks.state.current_top_level_url(),
        "https://news.example/article"
    );

    unsafe {
        on_address_change(
            display,
            browser,
            subframe.as_mut_ptr().cast(),
            subframe_url.as_ptr().cast::<CefString>(),
        );
    }
    assert_eq!(
        callbacks.state.current_top_level_url(),
        "https://news.example/article",
        "a later iframe navigation must not overwrite the committed page URL"
    );
}

#[test]
fn cef_navigation_generation_ignores_subframe_navigation_callbacks() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    browser
        [CEF_BROWSER_GET_MAIN_FRAME_OFFSET..CEF_BROWSER_GET_MAIN_FRAME_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(test_main_frame as *const () as usize).to_ne_bytes());
    let mut main = vec![0u8; CEF_FRAME_SIZE];
    let mut subframe = vec![0u8; CEF_FRAME_SIZE];
    TEST_MAIN_FRAME_PTR.store(main.as_mut_ptr() as usize, AtomicOrdering::SeqCst);
    let request = callbacks.state.request_ptr();
    let mut disable_default_handling = -1;
    let browser = browser.as_mut_ptr().cast();

    unsafe {
        get_resource_request_handler(
            request,
            browser,
            subframe.as_mut_ptr().cast(),
            ptr::null_mut(),
            1,
            0,
            ptr::null(),
            &mut disable_default_handling,
        );
    }
    assert_eq!(
        callbacks.state.navigations(),
        0,
        "subframe navigations should not trigger top-level reinjection cadence"
    );
    assert_eq!(disable_default_handling, 0);

    unsafe {
        get_resource_request_handler(
            request,
            browser,
            main.as_mut_ptr().cast(),
            ptr::null_mut(),
            1,
            0,
            ptr::null(),
            &mut disable_default_handling,
        );
    }
    assert_eq!(callbacks.state.navigations(), 1);
}

unsafe extern "C" fn record_key_event(_host: *mut c_void, event: *const c_void) {
    KEY_EVENT_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    // SAFETY: `send_char` passes a live `CefKeyEvent` byte block for the
    // duration of this synchronous fake callback.
    let bytes = unsafe { std::slice::from_raw_parts(event.cast::<u8>(), CEF_KEY_EVENT_SIZE) };
    KEY_EVENT_LAST_TYPE.store(
        read_i32(bytes, CEF_KEY_EVENT_TYPE_OFFSET),
        AtomicOrdering::SeqCst,
    );
    KEY_EVENT_LAST_WINDOWS_CODE.store(
        read_i32(bytes, CEF_KEY_EVENT_WINDOWS_KEY_CODE_OFFSET),
        AtomicOrdering::SeqCst,
    );
    KEY_EVENT_LAST_CHAR.store(
        i32::from(read_u16(bytes, CEF_KEY_EVENT_CHARACTER_OFFSET)),
        AtomicOrdering::SeqCst,
    );
}

#[test]
fn set_host_focus_uses_header_pinned_slot() {
    FOCUS_CALLS.store(0, AtomicOrdering::SeqCst);
    FOCUS_LAST.store(0, AtomicOrdering::SeqCst);
    let mut host = vec![0u8; CEF_BROWSER_HOST_SIZE];
    host[CEF_BROWSER_HOST_SET_FOCUS_OFFSET
        ..CEF_BROWSER_HOST_SET_FOCUS_OFFSET + std::mem::size_of::<usize>()]
        .copy_from_slice(&(record_focus as *const () as usize).to_ne_bytes());

    set_host_focus(host.as_mut_ptr().cast(), true);

    assert_eq!(FOCUS_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(FOCUS_LAST.load(AtomicOrdering::SeqCst), 1);
}

unsafe extern "C" fn record_stop_load(_browser: *mut c_void) {
    STOP_LOAD_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
}

#[test]
fn stop_control_uses_cef_stop_load_slot() {
    STOP_LOAD_CALLS.store(0, AtomicOrdering::SeqCst);
    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    browser
        [CEF_BROWSER_STOP_LOAD_OFFSET..CEF_BROWSER_STOP_LOAD_OFFSET + std::mem::size_of::<usize>()]
        .copy_from_slice(&(record_stop_load as *const () as usize).to_ne_bytes());
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");

    apply_control_frame(browser.as_mut_ptr().cast(), &callbacks, &ControlMsg::Stop);

    assert_eq!(STOP_LOAD_CALLS.load(AtomicOrdering::SeqCst), 1);
}

#[test]
fn child_handler_pointers_resolve_non_null_to_their_registered_block() {
    // REGRESSION: get_display_handler/get_load_handler/get_find_handler/
    // get_download_handler resolved via the size-keyed `lookup_peer`, whose
    // `callback_size` whitelist did NOT list the display(144)/load(72)/find(48)/
    // download(64) sizes (and find(48) aliases pdf_print(48)) — so they returned
    // NULL and CEF silently never dispatched on_address_change/title/favicon/
    // cursor, on_loading_state_change, on_find_result, or the download handler on
    // the LIVE vtable. Every feature still passed its unit tests because those
    // never exercise the real CEF callback path. Assert each child handler now
    // resolves DIRECTLY to its registered block (non-null), and that the
    // whitelisted peers did not regress.
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    assert_eq!(
        callbacks.state.display_ptr(),
        callbacks.display.as_mut_ptr()
    );
    assert_eq!(callbacks.state.load_ptr(), callbacks.load.as_mut_ptr());
    assert_eq!(callbacks.state.find_ptr(), callbacks.find.as_mut_ptr());
    assert_eq!(
        callbacks.state.download_ptr(),
        callbacks.download.as_mut_ptr()
    );
    assert_eq!(
        callbacks.state.jsdialog_ptr(),
        callbacks.jsdialog.as_mut_ptr()
    );
    for ptr in [
        callbacks.state.display_ptr(),
        callbacks.state.load_ptr(),
        callbacks.state.find_ptr(),
        callbacks.state.download_ptr(),
        callbacks.state.jsdialog_ptr(),
        // whitelisted peers must still resolve too (no regression):
        callbacks.state.render_ptr(),
        callbacks.state.request_ptr(),
    ] {
        assert!(!ptr.is_null(), "child handler must resolve to a live block");
    }

    // Demonstrate the EXACT pre-fix defect on the SAME live state: the old
    // resolution path — `lookup_peer(state, <size>)`, gated by `callback_size`'s
    // whitelist — returns NULL for display(144)/load(72)/download(64) because none
    // of those sizes is whitelisted. This is precisely the pointer CEF was handed
    // for get_display_handler/get_load_handler/get_download_handler before the fix,
    // so on the LIVE vtable CEF never dispatched on_address_change/on_title_change/
    // on_favicon_urlchange/on_cursor_change (display), on_loading_state_change
    // (load), or can_download/on_before_download (download) — while the unit tests,
    // which never exercise the real callback path, stayed green.
    for size in [
        CEF_DISPLAY_HANDLER_SIZE,
        CEF_LOAD_HANDLER_SIZE,
        CEF_DOWNLOAD_HANDLER_SIZE,
    ] {
        assert!(
            lookup_peer(&callbacks.state, size).is_null(),
            "the old size-keyed lookup_peer path returned NULL — the bug this fix replaces"
        );
    }
    // find(48) is a subtler case: 48 IS whitelisted (as pdf_print_callback(48)) and
    // the find handler is the only size-48 block registered at install, so the old
    // lookup happened to resolve it — but that is fragile (a later print_to_pdf
    // registers a second 48-block and the resolution becomes order-dependent). The
    // dedicated-pointer fix removes that fragility; the old path returned the right
    // block here only by coincidence.
    assert_eq!(
        lookup_peer(&callbacks.state, CEF_FIND_HANDLER_SIZE),
        callbacks.find.as_mut_ptr(),
        "find(48) resolved via the whitelisted pdf size — fragile but not null"
    );
    // ...and the genuinely-whitelisted peers resolved correctly all along.
    assert_eq!(
        lookup_peer(&callbacks.state, CEF_RENDER_HANDLER_SIZE),
        callbacks.render.as_mut_ptr()
    );
    assert_eq!(
        lookup_peer(&callbacks.state, CEF_REQUEST_HANDLER_SIZE),
        callbacks.request.as_mut_ptr()
    );
}

unsafe extern "C" fn record_audio_muted(_host: *mut c_void, muted: c_int) {
    AUDIO_MUTED_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    AUDIO_MUTED_LAST.store(muted, AtomicOrdering::SeqCst);
}

#[test]
fn audio_mute_control_uses_cef_host_audio_slot() {
    AUDIO_MUTED_CALLS.store(0, AtomicOrdering::SeqCst);
    AUDIO_MUTED_LAST.store(0, AtomicOrdering::SeqCst);

    let mut host = vec![0u8; CEF_BROWSER_HOST_SIZE];
    host[CEF_BROWSER_HOST_SET_AUDIO_MUTED_OFFSET
        ..CEF_BROWSER_HOST_SET_AUDIO_MUTED_OFFSET + std::mem::size_of::<usize>()]
        .copy_from_slice(&(record_audio_muted as *const () as usize).to_ne_bytes());
    TEST_HOST_PTR.store(host.as_mut_ptr() as usize, AtomicOrdering::SeqCst);

    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    browser
        [CEF_BROWSER_GET_HOST_OFFSET..CEF_BROWSER_GET_HOST_OFFSET + std::mem::size_of::<usize>()]
        .copy_from_slice(&(test_browser_host as *const () as usize).to_ne_bytes());
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");

    apply_control_frame(
        browser.as_mut_ptr().cast(),
        &callbacks,
        &ControlMsg::SetAudioMuted { muted: true },
    );

    assert_eq!(AUDIO_MUTED_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(AUDIO_MUTED_LAST.load(AtomicOrdering::SeqCst), 1);

    apply_control_frame(
        browser.as_mut_ptr().cast(),
        &callbacks,
        &ControlMsg::SetAudioMuted { muted: false },
    );

    assert_eq!(AUDIO_MUTED_CALLS.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(AUDIO_MUTED_LAST.load(AtomicOrdering::SeqCst), 0);
}

static IME_SET_COMP_CALLS: AtomicUsize = AtomicUsize::new(0);
static IME_SET_COMP_TEXT: Mutex<String> = Mutex::new(String::new());
static IME_SET_COMP_SEL_FROM: AtomicI32 = AtomicI32::new(-1);
static IME_SET_COMP_SEL_TO: AtomicI32 = AtomicI32::new(-1);
static IME_SET_COMP_UNDERLINES: AtomicI32 = AtomicI32::new(-1);
static IME_SET_COMP_UNDERLINES_NULL: AtomicI32 = AtomicI32::new(-1);
static IME_SET_COMP_REPLACEMENT_NULL: AtomicI32 = AtomicI32::new(-1);
static IME_COMMIT_CALLS: AtomicUsize = AtomicUsize::new(0);
static IME_COMMIT_TEXT: Mutex<String> = Mutex::new(String::new());
static IME_COMMIT_CURSOR: AtomicI32 = AtomicI32::new(i32::MIN);
static IME_COMMIT_REPLACEMENT_NULL: AtomicI32 = AtomicI32::new(-1);
static IME_FINISH_CALLS: AtomicUsize = AtomicUsize::new(0);
static IME_FINISH_KEEP: AtomicI32 = AtomicI32::new(-1);

/// Fake `cef_browser_host_t::ime_set_composition`: captures the composition text,
/// the underline count / NULL-ness, the NULL replacement range and the selection
/// range so the control path can be asserted without live CEF.
unsafe extern "C" fn record_ime_set_composition(
    _host: *mut c_void,
    text: *const c_void,
    underlines_count: usize,
    underlines: *const c_void,
    replacement_range: *const CefRange,
    selection_range: *const CefRange,
) {
    IME_SET_COMP_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    IME_SET_COMP_UNDERLINES.store(underlines_count as i32, AtomicOrdering::SeqCst);
    IME_SET_COMP_UNDERLINES_NULL.store(underlines.is_null() as i32, AtomicOrdering::SeqCst);
    IME_SET_COMP_REPLACEMENT_NULL.store(replacement_range.is_null() as i32, AtomicOrdering::SeqCst);
    if let Ok(mut guard) = IME_SET_COMP_TEXT.lock() {
        *guard = cef_string_to_string(text.cast::<CefString>());
    }
    // SAFETY: `ime_set_composition` always passes a live selection range for the
    // duration of the synchronous call.
    if !selection_range.is_null() {
        let range = unsafe { &*selection_range };
        IME_SET_COMP_SEL_FROM.store(range.from as i32, AtomicOrdering::SeqCst);
        IME_SET_COMP_SEL_TO.store(range.to as i32, AtomicOrdering::SeqCst);
    }
}

/// Fake `cef_browser_host_t::ime_commit_text`: captures the committed text, the
/// NULL replacement range and the relative cursor position.
unsafe extern "C" fn record_ime_commit_text(
    _host: *mut c_void,
    text: *const c_void,
    replacement_range: *const CefRange,
    relative_cursor_pos: c_int,
) {
    IME_COMMIT_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    IME_COMMIT_CURSOR.store(relative_cursor_pos, AtomicOrdering::SeqCst);
    IME_COMMIT_REPLACEMENT_NULL.store(replacement_range.is_null() as i32, AtomicOrdering::SeqCst);
    if let Ok(mut guard) = IME_COMMIT_TEXT.lock() {
        *guard = cef_string_to_string(text.cast::<CefString>());
    }
}

/// Fake `cef_browser_host_t::ime_finish_composing_text`: captures `keep_selection`.
unsafe extern "C" fn record_ime_finish_composing(_host: *mut c_void, keep_selection: c_int) {
    IME_FINISH_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    IME_FINISH_KEEP.store(keep_selection, AtomicOrdering::SeqCst);
}

#[test]
fn ime_controls_drive_cef_host_ime_slots() {
    IME_SET_COMP_CALLS.store(0, AtomicOrdering::SeqCst);
    IME_COMMIT_CALLS.store(0, AtomicOrdering::SeqCst);
    IME_FINISH_CALLS.store(0, AtomicOrdering::SeqCst);

    // Install the three IME fn-ptrs at their pinned host-vtable offsets.
    let mut host = vec![0u8; CEF_BROWSER_HOST_SIZE];
    host[CEF_BROWSER_HOST_IME_SET_COMPOSITION_OFFSET
        ..CEF_BROWSER_HOST_IME_SET_COMPOSITION_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(record_ime_set_composition as *const () as usize).to_ne_bytes());
    host[CEF_BROWSER_HOST_IME_COMMIT_TEXT_OFFSET
        ..CEF_BROWSER_HOST_IME_COMMIT_TEXT_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(record_ime_commit_text as *const () as usize).to_ne_bytes());
    host[CEF_BROWSER_HOST_IME_FINISH_COMPOSING_OFFSET
        ..CEF_BROWSER_HOST_IME_FINISH_COMPOSING_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(record_ime_finish_composing as *const () as usize).to_ne_bytes());
    TEST_HOST_PTR.store(host.as_mut_ptr() as usize, AtomicOrdering::SeqCst);

    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    browser[CEF_BROWSER_GET_HOST_OFFSET..CEF_BROWSER_GET_HOST_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(test_browser_host as *const () as usize).to_ne_bytes());
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let browser_ptr: *mut c_void = browser.as_mut_ptr().cast();

    // A preedit sets the composition with the caret at the end, zero underline
    // runs, a NULL underline array and a NULL replacement range. "かん" is two
    // UTF-16 code units, so the caret range is {2, 2}.
    apply_control_frame(
        browser_ptr,
        &callbacks,
        &ControlMsg::ImeSetComposition {
            text: "かん".into(),
        },
    );
    assert_eq!(IME_SET_COMP_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(*IME_SET_COMP_TEXT.lock().unwrap(), "かん");
    assert_eq!(IME_SET_COMP_SEL_FROM.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(IME_SET_COMP_SEL_TO.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(IME_SET_COMP_UNDERLINES.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(IME_SET_COMP_UNDERLINES_NULL.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        IME_SET_COMP_REPLACEMENT_NULL.load(AtomicOrdering::SeqCst),
        1
    );

    // An empty preedit STILL calls ime_set_composition (this is how a preedit is
    // cleared), with the caret collapsed to {0, 0}.
    apply_control_frame(
        browser_ptr,
        &callbacks,
        &ControlMsg::ImeSetComposition {
            text: String::new(),
        },
    );
    assert_eq!(IME_SET_COMP_CALLS.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(*IME_SET_COMP_TEXT.lock().unwrap(), "");
    assert_eq!(IME_SET_COMP_SEL_FROM.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(IME_SET_COMP_SEL_TO.load(AtomicOrdering::SeqCst), 0);

    // Commit routes to ime_commit_text with the text, a NULL replacement range and
    // a zero relative cursor position.
    apply_control_frame(
        browser_ptr,
        &callbacks,
        &ControlMsg::ImeCommitText {
            text: "漢字".into(),
        },
    );
    assert_eq!(IME_COMMIT_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(*IME_COMMIT_TEXT.lock().unwrap(), "漢字");
    assert_eq!(IME_COMMIT_CURSOR.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(IME_COMMIT_REPLACEMENT_NULL.load(AtomicOrdering::SeqCst), 1);

    // Finish routes to ime_finish_composing_text keeping the selection.
    apply_control_frame(browser_ptr, &callbacks, &ControlMsg::ImeFinishComposition);
    assert_eq!(IME_FINISH_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(IME_FINISH_KEEP.load(AtomicOrdering::SeqCst), 1);
}

#[test]
fn text_input_event_sends_cef_character_key_event() {
    FOCUS_CALLS.store(0, AtomicOrdering::SeqCst);
    FOCUS_LAST.store(0, AtomicOrdering::SeqCst);
    KEY_EVENT_CALLS.store(0, AtomicOrdering::SeqCst);
    KEY_EVENT_LAST_TYPE.store(-1, AtomicOrdering::SeqCst);
    KEY_EVENT_LAST_WINDOWS_CODE.store(-1, AtomicOrdering::SeqCst);
    KEY_EVENT_LAST_CHAR.store(-1, AtomicOrdering::SeqCst);

    let mut host = vec![0u8; CEF_BROWSER_HOST_SIZE];
    host[CEF_BROWSER_HOST_SET_FOCUS_OFFSET..CEF_BROWSER_HOST_SET_FOCUS_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(record_focus as *const () as usize).to_ne_bytes());
    host[CEF_BROWSER_HOST_SEND_KEY_EVENT_OFFSET
        ..CEF_BROWSER_HOST_SEND_KEY_EVENT_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(record_key_event as *const () as usize).to_ne_bytes());
    TEST_HOST_PTR.store(host.as_mut_ptr() as usize, AtomicOrdering::SeqCst);

    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    browser[CEF_BROWSER_GET_HOST_OFFSET..CEF_BROWSER_GET_HOST_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(test_browser_host as *const () as usize).to_ne_bytes());
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");

    apply_input_event(
        browser.as_mut_ptr().cast(),
        &callbacks,
        &InputEvent::Text("m".to_owned()),
    );

    assert_eq!(FOCUS_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(FOCUS_LAST.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(KEY_EVENT_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        KEY_EVENT_LAST_TYPE.load(AtomicOrdering::SeqCst),
        KEYEVENT_CHAR
    );
    assert_eq!(KEY_EVENT_LAST_WINDOWS_CODE.load(AtomicOrdering::SeqCst), 77);
    assert_eq!(KEY_EVENT_LAST_CHAR.load(AtomicOrdering::SeqCst), 109);
}

#[test]
fn text_input_uses_virtual_key_code_for_ascii_letters() {
    assert_eq!(super::char_windows_key_code(b'm' as u16), 77);
    assert_eq!(super::char_windows_key_code(b'M' as u16), 77);
    assert_eq!(super::char_windows_key_code(b'7' as u16), 55);
}

#[test]
fn audio_handler_publishes_audible_state_on_stream_start_and_stop() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        4,
        4,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    // Resolve the audio handler through the client vtable's get_audio_handler slot,
    // then drive its callbacks straight from the installed vtable offsets (proving
    // each fn-ptr was written to the right slot), not the Rust fns by name.
    let audio = unsafe { get_audio_handler(callbacks.client_ptr()) };
    assert!(!audio.is_null(), "get_audio_handler returned null");

    // get_audio_parameters MUST return non-zero (else CEF delivers no streams) and
    // fill the out-param with our sane STEREO / 48 kHz defaults.
    let get_params: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut CefAudioParameters,
    ) -> c_int = unsafe {
        std::mem::transmute(
            read_fn(audio, CEF_AUDIO_HANDLER_GET_AUDIO_PARAMETERS_OFFSET)
                .expect("get_audio_parameters slot"),
        )
    };
    let mut params = CefAudioParameters {
        size: 0,
        channel_layout: 0,
        sample_rate: 0,
        frames_per_buffer: 0,
    };
    let rv = unsafe { get_params(audio, ptr::null_mut(), &mut params) };
    assert_ne!(rv, 0, "get_audio_parameters must return true");
    // `size` first: if the struct layout regressed to omit it, channel_layout
    // would alias `size` and this equality would fail.
    assert_eq!(params.size, CEF_AUDIO_PARAMETERS_SIZE);
    assert_eq!(params.channel_layout, CEF_CHANNEL_LAYOUT_STEREO);
    assert!(params.sample_rate > 0 && params.frames_per_buffer > 0);

    // Stream started → AudioState { audible: true }.
    let on_started: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *const CefAudioParameters,
        c_int,
    ) = unsafe {
        std::mem::transmute(
            read_fn(audio, CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STARTED_OFFSET)
                .expect("on_audio_stream_started slot"),
        )
    };
    unsafe { on_started(audio, ptr::null_mut(), &params, 2) };
    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("audio started recv") else {
        panic!("expected audio started event")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::AudioState { audible: true }
    );

    // Stream stopped → AudioState { audible: false }.
    let on_stopped: unsafe extern "C" fn(*mut c_void, *mut c_void) = unsafe {
        std::mem::transmute(
            read_fn(audio, CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STOPPED_OFFSET)
                .expect("on_audio_stream_stopped slot"),
        )
    };
    unsafe { on_stopped(audio, ptr::null_mut()) };
    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("audio stopped recv") else {
        panic!("expected audio stopped event")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::AudioState { audible: false }
    );
}

static SET_HEADER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SET_HEADER_OVERWRITE: AtomicI32 = AtomicI32::new(-1);
static SET_HEADER_NAME: Mutex<String> = Mutex::new(String::new());
static SET_HEADER_VALUE: Mutex<String> = Mutex::new(String::new());

/// Fake `cef_request_t::set_header_by_name`: records the header name/value and the
/// overwrite flag so the before-load path can be asserted without live CEF.
unsafe extern "C" fn record_set_header(
    _request: *mut c_void,
    name: *const c_void,
    value: *const c_void,
    overwrite: c_int,
) {
    SET_HEADER_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    SET_HEADER_OVERWRITE.store(overwrite, AtomicOrdering::SeqCst);
    // SAFETY: the before-load path passes live `CefStringOwned` pointers for the
    // header name and value, valid for the duration of this call.
    let read = |raw: *const c_void| -> String {
        let raw = raw.cast::<CefString>();
        unsafe {
            if raw.is_null() || (*raw).str_.is_null() || (*raw).length == 0 {
                String::new()
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts((*raw).str_, (*raw).length))
            }
        }
    };
    if let Ok(mut guard) = SET_HEADER_NAME.lock() {
        *guard = read(name);
    }
    if let Ok(mut guard) = SET_HEADER_VALUE.lock() {
        *guard = read(value);
    }
}

#[test]
fn user_agent_override_stamps_real_http_header_in_before_load() {
    SET_HEADER_CALLS.store(0, AtomicOrdering::SeqCst);
    SET_HEADER_OVERWRITE.store(-1, AtomicOrdering::SeqCst);
    SET_HEADER_NAME.lock().unwrap().clear();
    SET_HEADER_VALUE.lock().unwrap().clear();

    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");

    // A fake, mutable cef_request_t whose set_header_by_name slot records the call.
    // Its get_url slot is left null: request_url() then yields None and the path
    // returns RV_CONTINUE — but the header stamp happens first, which is the SUT.
    let mut request = vec![0u8; CEF_REQUEST_SIZE];
    request[CEF_REQUEST_SET_HEADER_BY_NAME_OFFSET
        ..CEF_REQUEST_SET_HEADER_BY_NAME_OFFSET + size_of::<usize>()]
        .copy_from_slice(&(record_set_header as *const () as usize).to_ne_bytes());

    // `self_` is the resource-request handler block; with_state() resolves it to
    // this browser's state via the callback registry, exactly as live CEF does.
    let self_ = callbacks.state.resource_request_ptr();
    assert!(!self_.is_null(), "resource-request handler must resolve");

    // 1) No override stored → the before-load path must NOT touch the header.
    unsafe {
        on_before_resource_load(
            self_,
            ptr::null_mut(),
            ptr::null_mut(),
            request.as_mut_ptr().cast(),
            ptr::null_mut(),
        );
    }
    assert_eq!(
        SET_HEADER_CALLS.load(AtomicOrdering::SeqCst),
        0,
        "an empty User-Agent override must leave the request header untouched"
    );

    // 2) Store an override through the real control path, then the before-load path
    //    must stamp `User-Agent: <ua>` with overwrite=1. The zeroed fake browser
    //    makes apply_user_agent()'s JS injection a harmless no-op (no main frame).
    const UA: &str = "Mozilla/5.0 (X11; Fedora; Linux x86_64) MDE/1.0 Chrome/149.0 Safari/537.36";
    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    apply_control_frame(
        browser.as_mut_ptr().cast(),
        &callbacks,
        &ControlMsg::SetUserAgent {
            user_agent: UA.to_owned(),
        },
    );

    unsafe {
        on_before_resource_load(
            self_,
            ptr::null_mut(),
            ptr::null_mut(),
            request.as_mut_ptr().cast(),
            ptr::null_mut(),
        );
    }
    assert_eq!(
        SET_HEADER_CALLS.load(AtomicOrdering::SeqCst),
        1,
        "a stored override stamps the real HTTP User-Agent header exactly once"
    );
    assert_eq!(
        SET_HEADER_OVERWRITE.load(AtomicOrdering::SeqCst),
        1,
        "overwrite=1 replaces Chromium's default User-Agent"
    );
    assert_eq!(&*SET_HEADER_NAME.lock().unwrap(), "User-Agent");
    assert_eq!(&*SET_HEADER_VALUE.lock().unwrap(), UA);
}

static PRINT_CALLS: AtomicUsize = AtomicUsize::new(0);
static PDF_CALLS: AtomicUsize = AtomicUsize::new(0);
static PDF_PATH_LEN: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn record_print(_host: *mut c_void) {
    PRINT_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
}

unsafe extern "C" fn record_print_to_pdf(
    _host: *mut c_void,
    path: *const c_void,
    settings: *const c_void,
    callback: *mut c_void,
) {
    PDF_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    assert!(
        settings.is_null(),
        "default PDF settings are passed as null"
    );
    assert!(!callback.is_null(), "PDF completion callback is retained");
    let path = path.cast::<CefString>();
    assert!(!path.is_null(), "PDF path is a CEF string");
    // SAFETY: the test passes a live CefStringOwned for this call.
    let len = unsafe { (*path).length };
    PDF_PATH_LEN.store(len, AtomicOrdering::SeqCst);
}

#[test]
fn print_controls_use_cef_host_print_slots() {
    PRINT_CALLS.store(0, AtomicOrdering::SeqCst);
    PDF_CALLS.store(0, AtomicOrdering::SeqCst);
    PDF_PATH_LEN.store(0, AtomicOrdering::SeqCst);

    let mut host = vec![0u8; CEF_BROWSER_HOST_SIZE];
    host[CEF_BROWSER_HOST_PRINT_OFFSET
        ..CEF_BROWSER_HOST_PRINT_OFFSET + std::mem::size_of::<usize>()]
        .copy_from_slice(&(record_print as *const () as usize).to_ne_bytes());
    host[CEF_BROWSER_HOST_PRINT_TO_PDF_OFFSET
        ..CEF_BROWSER_HOST_PRINT_TO_PDF_OFFSET + std::mem::size_of::<usize>()]
        .copy_from_slice(&(record_print_to_pdf as *const () as usize).to_ne_bytes());
    TEST_HOST_PTR.store(host.as_mut_ptr() as usize, AtomicOrdering::SeqCst);

    let mut browser = vec![0u8; CEF_BROWSER_SIZE];
    browser
        [CEF_BROWSER_GET_HOST_OFFSET..CEF_BROWSER_GET_HOST_OFFSET + std::mem::size_of::<usize>()]
        .copy_from_slice(&(test_browser_host as *const () as usize).to_ne_bytes());
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");

    apply_control_frame(
        browser.as_mut_ptr().cast(),
        &callbacks,
        &ControlMsg::PrintPage,
    );
    assert_eq!(PRINT_CALLS.load(AtomicOrdering::SeqCst), 1);

    apply_control_frame(
        browser.as_mut_ptr().cast(),
        &callbacks,
        &ControlMsg::SavePdf {
            path: "/tmp/mde-browser-page.pdf".to_owned(),
        },
    );
    assert_eq!(PDF_CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        PDF_PATH_LEN.load(AtomicOrdering::SeqCst),
        "/tmp/mde-browser-page.pdf".encode_utf16().count()
    );
}

#[test]
fn print_handler_returns_letter_paper_and_cancels_interactive_jobs() {
    assert_eq!(
        unsafe { get_pdf_paper_size(ptr::null_mut(), ptr::null_mut(), 72) },
        CefSize {
            width: 612,
            height: 792
        }
    );
    assert_eq!(
        unsafe { on_print_dialog(ptr::null_mut(), ptr::null_mut(), 0, ptr::null_mut()) },
        0
    );
    assert_eq!(
        unsafe {
            on_print_job(
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
            )
        },
        0
    );
}

#[test]
fn input_mapping_helpers_cover_wire_keys_and_modifiers() {
    assert_eq!(windows_key_code(KeyCode::Enter), Some(13));
    assert_eq!(windows_key_code(KeyCode::A), Some(65));
    assert_eq!(windows_key_code(KeyCode::Num9), Some(57));
    assert_eq!(windows_key_code(KeyCode::F12), Some(123));
    assert_eq!(
        cef_modifiers(Modifiers(
            Modifiers::CTRL | Modifiers::SHIFT | Modifiers::COMMAND
        )),
        EVENTFLAG_CONTROL_DOWN | EVENTFLAG_SHIFT_DOWN | EVENTFLAG_COMMAND_DOWN
    );
    assert_eq!(f32_to_i32(12.4), 12);
    assert_eq!(f32_to_i32(12.5), 13);
    assert_eq!(f32_to_i32(f32::NAN), 0);
}

#[test]
fn cosmetic_filter_script_installs_and_clears_the_style_element() {
    let script = cosmetic_filter_script(r#"#ad, .sponsor::before { content: "\"x\""; }"#);
    assert!(script.contains("mde-cef-cosmetic-style"));
    assert!(script.contains("document.createElement('style')"));
    assert!(script.contains(r#"#ad, .sponsor::before { content: \"\\\"x\\\"\"; }"#));

    let clear = cosmetic_filter_script("");
    assert!(clear.contains("if(el)el.remove();return;"));
}

#[test]
fn native_zoom_level_follows_chromium_convention_and_clamps() {
    // zoom_level = ln(factor)/ln(1.2): 100% → 0, 120% → 1 step, ~144% → 2 steps.
    assert!(zoom_level_for_percent(100).abs() < 1e-9);
    assert!((zoom_level_for_percent(120) - 1.0).abs() < 1e-9);
    assert!((zoom_level_for_percent(144) - 2.0).abs() < 0.01);
    // Below-100% zooms are negative levels.
    assert!(zoom_level_for_percent(50) < 0.0);
    // The shell's 25–500% bounds hold even for wild inputs (and ln(0) can never
    // be reached — percent 0 clamps to 25%).
    assert_eq!(zoom_level_for_percent(5), zoom_level_for_percent(25));
    assert_eq!(zoom_level_for_percent(900), zoom_level_for_percent(500));
    assert!(zoom_level_for_percent(0).is_finite());
    // Offset reconciliation: set_zoom_level is field 15 of cef_browser_host_t,
    // anchored by the proven close_browser=48 (field 1) and set_focus=72 (field 4).
    assert_eq!(CEF_BROWSER_HOST_SET_ZOOM_LEVEL_OFFSET, 40 + 15 * 8);
}

#[test]
fn download_handler_offsets_reconcile_and_size_is_unique() {
    // get_download_handler is client field 5 (get_display=72/field4 anchors it).
    assert_eq!(CEF_CLIENT_GET_DOWNLOAD_HANDLER_OFFSET, 40 + 5 * 8);
    // cef_download_handler_t: can_download(40), on_before_download(48),
    // on_download_updated(56) → 3 fields, size 64.
    assert_eq!(CEF_DOWNLOAD_HANDLER_SIZE, 40 + 3 * 8);
    assert_eq!(CEF_DOWNLOAD_HANDLER_CAN_DOWNLOAD_OFFSET, 40);
    assert_eq!(CEF_DOWNLOAD_HANDLER_ON_BEFORE_DOWNLOAD_OFFSET, 48);
    // Item getters (verified against cef_download_item_capi.h).
    assert_eq!(CEF_DOWNLOAD_ITEM_GET_URL_OFFSET, 40 + 14 * 8);
    assert_eq!(
        CEF_DOWNLOAD_ITEM_GET_SUGGESTED_FILE_NAME_OFFSET,
        40 + 16 * 8
    );
    // Size 64 must stay unique for lookup_peer resolution.
    for other in [
        CEF_LIFE_SPAN_HANDLER_SIZE,
        CEF_RENDER_HANDLER_SIZE,
        CEF_REQUEST_HANDLER_SIZE,
        CEF_RESOURCE_REQUEST_HANDLER_SIZE,
        CEF_DISPLAY_HANDLER_SIZE,
        CEF_LOAD_HANDLER_SIZE,
        CEF_FIND_HANDLER_SIZE,
    ] {
        assert_ne!(CEF_DOWNLOAD_HANDLER_SIZE, other);
    }
}

#[test]
fn frame_edit_command_offsets_match_the_cef149_frame_layout() {
    // cef_frame_t: 40-byte base then 8-byte fn ptrs. is_valid=0(40), undo=1(48),
    // redo=2(56), cut=3(64), copy=4(72), paste=5(80), del=7(96), select_all=8(104),
    // get_source=10(120), get_text=11(128), load_url=13(144), execute_js=14(152),
    // is_main=15(160).
    assert_eq!(CEF_FRAME_UNDO_OFFSET, 40 + 1 * 8);
    assert_eq!(CEF_FRAME_REDO_OFFSET, 40 + 2 * 8);
    assert_eq!(CEF_FRAME_CUT_OFFSET, 40 + 3 * 8);
    assert_eq!(CEF_FRAME_COPY_OFFSET, 40 + 4 * 8);
    assert_eq!(CEF_FRAME_PASTE_OFFSET, 40 + 5 * 8);
    assert_eq!(CEF_FRAME_DELETE_OFFSET, 40 + 7 * 8);
    assert_eq!(CEF_FRAME_SELECT_ALL_OFFSET, 40 + 8 * 8);
    assert_eq!(CEF_FRAME_GET_TEXT_OFFSET, 40 + 11 * 8);
    assert_eq!(CEF_FRAME_LOAD_URL_OFFSET, 40 + 13 * 8);
    assert_eq!(CEF_FRAME_EXECUTE_JAVA_SCRIPT_OFFSET, 40 + 14 * 8);
    assert_eq!(CEF_FRAME_IS_MAIN_OFFSET, 40 + 15 * 8);
    // Reconcile with the frame's execute_java_script slot — edit commands sit
    // before the page-tool extraction/navigation slots.
    assert!(CEF_FRAME_SELECT_ALL_OFFSET < CEF_FRAME_EXECUTE_JAVA_SCRIPT_OFFSET);
    assert!(CEF_FRAME_EXECUTE_JAVA_SCRIPT_OFFSET < CEF_FRAME_IS_MAIN_OFFSET);
}

#[test]
fn find_handler_offsets_reconcile_with_the_client_and_host_layout() {
    // get_find_handler is client field 7 (get_display=72/field4 + get_download=80
    // /field5 pin the run); the 48-byte 1-field handler holds on_find_result at 40.
    assert_eq!(CEF_CLIENT_GET_FIND_HANDLER_OFFSET, 40 + 7 * 8);
    assert_eq!(CEF_FIND_HANDLER_SIZE, 40 + 1 * 8);
    assert_eq!(CEF_FIND_HANDLER_ON_FIND_RESULT_OFFSET, 40);
    // host find/stop_finding are fields 21/22 (set_zoom_level=160/field15 anchors).
    assert_eq!(CEF_BROWSER_HOST_FIND_OFFSET, 40 + 21 * 8);
    assert_eq!(CEF_BROWSER_HOST_STOP_FINDING_OFFSET, 40 + 22 * 8);
    // Find-handler size 48 must stay unique for lookup_peer resolution.
    for other in [
        CEF_LIFE_SPAN_HANDLER_SIZE,
        CEF_RENDER_HANDLER_SIZE,
        CEF_REQUEST_HANDLER_SIZE,
        CEF_RESOURCE_REQUEST_HANDLER_SIZE,
        CEF_DISPLAY_HANDLER_SIZE,
        CEF_LOAD_HANDLER_SIZE,
    ] {
        assert_ne!(CEF_FIND_HANDLER_SIZE, other);
    }
}

#[test]
fn force_dark_script_installs_and_clears_bounded_style() {
    let enable = force_dark_script(true);
    assert!(enable.contains("mde-cef-force-dark-style"));
    assert!(enable.contains("colorScheme='dark'"));
    assert!(enable.contains("img, video, canvas"));
    assert!(
        !enable.contains("<script"),
        "force-dark is injected as style text only"
    );

    let disable = force_dark_script(false);
    assert!(disable.contains("if(el)el.remove()"));
    assert!(disable.contains("colorScheme=''"));
}

#[test]
fn reader_mode_script_installs_and_clears_bounded_style() {
    let enable = reader_mode_script(true);
    assert!(enable.contains("mde-cef-reader-style"));
    assert!(enable.contains("mde-reader-mode"));
    assert!(enable.contains("max-width: 76ch"));
    assert!(
        !enable.contains("<script"),
        "reader mode is injected as style text only"
    );

    let disable = reader_mode_script(false);
    assert!(disable.contains("if(el)el.remove()"));
    assert!(disable.contains("classList.remove"));
}

#[test]
fn autoplay_block_script_patches_media_play_and_cleans_up() {
    let enable = autoplay_block_script(true);
    assert!(enable.contains("__mdeAutoplayBlocker"));
    assert!(enable.contains("mdeAutoplayBlocked"));
    assert!(enable.contains("HTMLMediaElement.prototype"));
    assert!(enable.contains("MutationObserver"));
    assert!(enable.contains("removeAttribute('autoplay')"));
    assert!(enable.contains("Promise.reject"));
    assert!(
        !enable.contains("</script>"),
        "autoplay blocking is injected as bounded script text only"
    );

    let disable = autoplay_block_script(false);
    assert!(disable.contains("observer.disconnect"));
    assert!(disable.contains("HTMLMediaElement.prototype.play=s.originalPlay"));
    assert!(disable.contains("delete window.__mdeAutoplayBlocker"));
    assert!(disable.contains("delete document.documentElement.dataset.mdeAutoplayBlocked"));
}

#[test]
fn media_playback_toggle_script_drives_html_media_elements() {
    let script = media_playback_toggle_script();
    assert!(script.contains("querySelectorAll('audio,video')"));
    assert!(script.contains("pause()"));
    assert!(script.contains("play()"));
    assert!(script.contains("mdeAutoplayAllowed"));
    assert!(
        !script.contains("</script>"),
        "media transport is injected as bounded script text only"
    );
}

#[test]
fn media_transport_script_covers_page_media_actions() {
    for (action, token) in [
        (MediaTransportAction::PlayPause, "playPause"),
        (MediaTransportAction::Play, "play"),
        (MediaTransportAction::Pause, "pause"),
        (MediaTransportAction::Stop, "stop"),
        (MediaTransportAction::Next, "next"),
        (MediaTransportAction::Previous, "previous"),
        (MediaTransportAction::VolumeUp, "volumeUp"),
        (MediaTransportAction::VolumeDown, "volumeDown"),
    ] {
        let script = media_transport_script(action);
        assert!(script.contains("querySelectorAll('audio,video')"));
        assert!(script.contains(&format!("action='{token}'")));
        assert!(script.contains("pauseActive"));
        assert!(script.contains("fastSeek"));
        assert!(script.contains("volume(current"));
        assert!(script.contains("mdeAutoplayAllowed"));
        assert!(
            !script.contains("</script>"),
            "media transport is injected as bounded script text only"
        );
    }
}

#[test]
fn media_metadata_beacon_script_is_bounded_and_decodable() {
    let script = media_metadata_beacon_script();
    assert!(script.contains("navigator.mediaSession"));
    assert!(script.contains("querySelectorAll('audio,video')"));
    assert!(script.contains("duration_ms"));
    assert!(script.contains("position_ms"));
    assert!(script.contains("volume_percent"));
    assert!(script.contains("__mdeMediaMetadataLast"));
    assert!(script.contains(CEF_MEDIA_METADATA_BEACON_PREFIX));
    assert!(script.contains("encodeURIComponent(body)"));
    assert!(script.contains("publish('')"));
    assert!(
        !script.contains("</script>"),
        "media metadata is injected as bounded script text only"
    );

    let body = decode_media_metadata_beacon(
        "https://mde-media.invalid/metadata/?body=%7B%22title%22%3A%22Track%22%2C%22paused%22%3Afalse%7D",
    )
    .expect("media metadata beacon");
    assert_eq!(body, r#"{"title":"Track","paused":false}"#);
    assert_eq!(
        decode_media_metadata_beacon("https://mde-media.invalid/metadata/?body=not-json"),
        None
    );
    assert_eq!(
        decode_media_metadata_beacon("https://mde-media.invalid/metadata/?body="),
        Some(String::new())
    );
    assert_eq!(decode_media_metadata_beacon("https://example.com/"), None);
}

#[test]
fn media_metadata_playing_state_is_private_and_conservative() {
    assert!(media_metadata_reports_playing(
        r#"{"title":"Track","paused":false}"#
    ));
    assert!(media_metadata_reports_playing(
        r#"{"title":"Track","artist":"Artist","paused" : false,"position_ms":42}"#
    ));
    assert!(!media_metadata_reports_playing(
        r#"{"title":"Track","paused":true}"#
    ));
    assert!(!media_metadata_reports_playing(r#"{"title":"Track"}"#));
    assert!(!media_metadata_reports_playing(""));
}

#[test]
fn autoplay_block_control_is_remembered_for_navigation_reinjection() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    assert!(!callbacks.state.autoplay_blocked.load(Ordering::SeqCst));

    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::SetAutoplayBlocked { blocked: true },
    );
    assert!(
        callbacks.state.autoplay_blocked.load(Ordering::SeqCst),
        "CEF must remember autoplay blocking so fresh documents get the shim"
    );

    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::SetAutoplayBlocked { blocked: false },
    );
    assert!(
        !callbacks.state.autoplay_blocked.load(Ordering::SeqCst),
        "disabling autoplay blocking must stop navigation reinjection"
    );
}

#[test]
fn user_agent_override_script_installs_and_clears_page_visible_ua() {
    let enable = user_agent_override_script("Mozilla/5.0 MDE-Test");
    assert!(enable.contains("Navigator.prototype"));
    assert!(enable.contains("userAgent"));
    assert!(enable.contains("MDE-Test"));
    assert!(
        !enable.contains("</script>"),
        "UA override is injected as bounded script text only"
    );

    let disable = user_agent_override_script("");
    assert!(disable.contains("__mdeUserAgentOverride"));
    assert!(disable.contains("delete"));
}

#[test]
fn login_fill_script_targets_the_login_form_and_dispatches_events() {
    let s = login_fill_script("alice@example.com", "hunter2");
    // Finds the password field, prefers an autocomplete=username field, sets both.
    assert!(s.contains("input[type=password]"));
    assert!(s.contains("autocomplete=username"));
    assert!(s.contains("alice@example.com"));
    assert!(s.contains("hunter2"));
    // Fires input+change so the page's own JS observes the autofill.
    assert!(s.contains("dispatchEvent"));
    assert!(s.contains("input"));
    assert!(s.contains("change"));
    // Injected as bounded script text only — no tag-breakout.
    assert!(!s.contains("</script>"));
}

#[test]
fn login_capture_script_installs_an_idempotent_submit_beacon() {
    let s = login_capture_script();
    assert!(s.contains("addEventListener('submit'"));
    assert!(s.contains("input[type=password]"));
    assert!(s.contains("mde-login.invalid/capture/")); // beacons to the intercepted URL
    assert!(s.contains("__mdeLoginCaptureInstalled")); // install-once guard
    assert!(s.contains("location.origin"));
    assert!(s.contains("origin="));
    assert!(s.contains("username:u?u.value:''"));
    assert!(!s.contains("</script>"));
}

#[test]
fn decode_login_beacon_extracts_json_and_rejects_non_login_urls() {
    let (origin, body) = decode_login_beacon(
        "https://mde-login.invalid/capture/?origin=https%3A%2F%2Flogin.example&body=%7B%22ok%22%3A1%7D",
    )
    .expect("a login beacon decodes");
    assert_eq!(origin, "https://login.example");
    assert_eq!(body, "{\"ok\":1}");
    // Non-login URLs (incl. the passkey beacon) are ignored.
    assert!(decode_login_beacon("https://example.com/login").is_none());
    assert!(decode_login_beacon("https://mde-passkey.invalid/request/?body=%7B%7D").is_none());
    // Missing origin is rejected: the engine must bind capture to a real page.
    assert!(decode_login_beacon("https://mde-login.invalid/capture/?body=%7B%7D").is_none());
    // A body that isn't a JSON object is rejected.
    assert!(decode_login_beacon(
        "https://mde-login.invalid/capture/?origin=https%3A%2F%2Flogin.example&body=notjson"
    )
    .is_none());
}

#[test]
fn login_capture_origin_must_match_the_top_level_page() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    *callbacks.state.nav_url.lock().expect("nav") =
        "https://mail.example.com/account/login".to_owned();

    assert!(callbacks
        .state
        .login_beacon_matches_top_level("https://mail.example.com"));
    assert!(callbacks.state.host_matches_top_level("mail.example.com"));
    assert!(
        !callbacks
            .state
            .login_beacon_matches_top_level("https://bank.example"),
        "a forged login-capture origin must not be accepted for another host"
    );
    assert!(
        !callbacks.state.host_matches_top_level("bank.example"),
        "a stale FillLogin host must not inject into the current page"
    );
}

#[test]
fn device_profile_script_installs_and_clears_page_visible_device_metadata() {
    let enable = device_profile_script("phone", 390, 844, 300, true);
    assert!(enable.contains("mdeDeviceProfile"));
    assert!(enable.contains("innerWidth"));
    assert!(enable.contains("maxTouchPoints"));
    assert!(enable.contains("mde-device-profile-viewport"));
    assert!(enable.contains("width:390"));
    assert!(
        !enable.contains("</script>"),
        "device profile is injected as bounded script text only"
    );

    let disable = device_profile_script("default", 0, 0, 100, false);
    assert!(disable.contains("__mdeDeviceProfile"));
    assert!(disable.contains("delete"));
}

#[test]
fn userscript_library_script_runs_and_cleans_the_bundle() {
    let enable = userscript_library_script(
        true,
        "document.documentElement.dataset.curatedUserscript='npr';",
    );
    assert!(enable.contains("mdeBrowserUserscripts"));
    assert!(enable.contains("curatedUserscript='npr'"));
    assert!(enable.contains("try{"));

    let disable = userscript_library_script(false, "");
    assert!(disable.contains("mde-browser-userscript-style"));
    assert!(disable.contains("__mdeBrowserUserScriptsObserver"));
    assert!(disable.contains("delete document.documentElement.dataset.mdeBrowserUserscripts"));
}

#[test]
fn spellcheck_highlight_script_marks_and_clears_words() {
    let script =
        spellcheck_highlight_script(&["wrold".to_owned(), "mesh?".to_owned(), "x".to_owned()]);
    assert!(script.contains("mde-browser-spell-miss"));
    assert!(script.contains("mde-browser-spellcheck-style"));
    assert!(script.contains("\"wrold\""));
    assert!(script.contains("\"mesh?\""));
    assert!(!script.contains("\"x\""));

    let clear = spellcheck_highlight_script(&[]);
    assert!(clear.contains("delete document.documentElement.dataset.mdeBrowserSpellcheck"));
}

#[test]
fn spellcheck_correction_script_replaces_mark_or_text() {
    let script = spellcheck_correction_script("wrold", "world");
    assert!(script.contains("mde-browser-spell-miss"));
    assert!(script.contains(r#"word="wrold""#));
    assert!(script.contains(r#"replacement="world""#));
    assert!(script.contains("replaceWith(document.createTextNode(replacement))"));
    assert!(script.contains("createTreeWalker"));
    assert!(script.contains("var targetOccurrence=0"));
    assert!(script.contains("var replaceAll=targetOccurrence<0"));

    let all = spellcheck_correction_all_script("wrold", "world");
    assert!(all.contains("var targetOccurrence=-1"));
    assert!(all.contains("while((node=walker.nextNode())&&total<512)"));
    assert!(all.contains("text.replace(re,function(m)"));

    let indexed = spellcheck_correction_at_script("wrold", "world", 3);
    assert!(indexed.contains("var targetOccurrence=3"));
    assert!(indexed.contains("if(total===targetOccurrence)"));
    assert!(indexed.contains("if(markMatches>0&&!replaceAll)return"));

    assert_eq!(spellcheck_correction_script("", "world"), "()=>{}");
    assert_eq!(spellcheck_correction_script("wrold", ""), "()=>{}");
    assert_eq!(spellcheck_correction_all_script("", "world"), "()=>{}");
    assert_eq!(spellcheck_correction_at_script("", "world", 1), "()=>{}");
}

#[test]
fn page_text_beacon_script_is_bounded_and_decodable() {
    let script = page_text_beacon_script(42, 200_000);
    assert!(script.contains("cap=8192"));
    assert!(script.contains("innerText||root.textContent"));
    assert!(script.contains("encodeURIComponent(text)"));
    assert!(script.contains("https://mde-page-text.invalid/capture/42?text="));
    assert!(script.contains("window.alert(u)"));

    assert_eq!(
        decode_page_text_beacon("https://mde-page-text.invalid/capture/42?text=hello%20w%C3%B8rld"),
        Some((42, "hello wørld".to_owned()))
    );
    assert_eq!(
        decode_page_text_beacon("mde-page-text://capture/42?text=hello%20w%C3%B8rld"),
        Some((42, "hello wørld".to_owned()))
    );
    assert_eq!(percent_decode("%zz+ok"), "%zz+ok");
    assert_eq!(clamp_utf8("abé", 3), "ab");
    assert_eq!(decode_page_text_beacon("https://example.com/"), None);
}

#[test]
fn page_text_script_can_be_dispatched_as_a_javascript_url() {
    let script = page_text_beacon_script(42, 256);
    let url = javascript_url_for_script(&script);
    assert!(url.starts_with("javascript:(function()%7B"));
    assert!(url.contains("mde-page-text.invalid"));
    assert!(!url.contains(' '));
    assert!(!url.contains('\n'));
}

#[test]
fn page_scrape_beacon_script_is_bounded_and_decodable() {
    let script = page_scrape_beacon_script(43, 200_000, 400, 300);
    assert!(script.contains("textCap=Math.min(32768,16384)"));
    assert!(script.contains("articleCap=8192"));
    assert!(script.contains("linkCap=128"));
    assert!(script.contains("headingCap=64"));
    assert!(script.contains("querySelectorAll('a[href]')"));
    assert!(script.contains("querySelectorAll('h1,h2,h3,h4,h5,h6')"));
    assert!(script.contains("querySelectorAll('article,main,[role=main]')"));
    assert!(script.contains("link[rel~=\"canonical\"][href]"));
    assert!(script.contains("meta[name=\"description\" i][content]"));
    assert!(script.contains("links=links.slice(0,32)"));
    assert!(script.contains("https://mde-page-scrape.invalid/capture/43?body="));
    assert!(script.contains("window.alert(u)"));

    assert_eq!(
        decode_page_scrape_beacon(
            "https://mde-page-scrape.invalid/capture/43?body=%7B%22text%22%3A%22hello%22%7D"
        ),
        Some((43, r#"{"text":"hello"}"#.to_owned()))
    );
    assert_eq!(decode_page_scrape_beacon("https://example.com/"), None);
}

#[test]
fn passkey_bridge_script_uses_bounded_beacon_metadata() {
    let script = passkey_bridge_script();
    assert!(script.contains("navigator.credentials"));
    assert!(script.contains("creds.create=function"));
    assert!(script.contains("creds.get=function"));
    // browser-4: capture the originals so non-publicKey requests fall
    // through instead of being hijacked into a passkey ceremony.
    assert!(script.contains("origCreate"));
    assert!(script.contains("origGet"));
    // security-2(a): only dispatch behind a real user gesture (transient
    // activation), and thread the presence signal to the daemon.
    assert!(script.contains("hasUserGesture"));
    assert!(script.contains("navigator.userActivation"));
    assert!(script.contains("user_present"));
    assert!(script.contains(CEF_PASSKEY_BEACON_PREFIX));
    assert!(script.contains("challenge_b64url"));
    assert!(script.contains("allow_credentials"));
    assert!(script.contains("client_request_id"));
    assert!(script.contains("__mdeBrowserPasskeyComplete"));
    assert!(script.contains("pending.resolve"));
    assert!(script.contains("public_key_spki_der_b64url"));
    assert!(script.contains("getClientExtensionResults"));
    assert!(script.contains("isUserVerifyingPlatformAuthenticatorAvailable"));
    assert!(script.contains("isConditionalMediationAvailable"));
    assert!(script.contains("Object.setPrototypeOf"));
    assert!(script.contains("getTransports"));
    assert!(script.contains("toJSON"));

    let complete = passkey_complete_script(r#"{"client_request_id":"pk-1"}"#);
    assert!(complete.contains("__mdeBrowserPasskeyComplete"));
    assert!(complete.contains("JSON.parse"));

    let body = decode_passkey_beacon(
        "https://mde-passkey.invalid/request/?body=%7B%22ceremony%22%3A%22get%22%7D",
    )
    .expect("passkey beacon");
    assert_eq!(body, r#"{"ceremony":"get"}"#);
    assert_eq!(decode_passkey_beacon("https://example.test/"), None);
}

#[test]
fn passkey_shim_feature_detects_credential_type_and_gates_on_a_gesture() {
    // browser-4: a plain `navigator.credentials.get({password:true})`
    // (password-manager autofill), `{federated:...}`, or WebOTP `{otp:...}`
    // request has no `publicKey` member, so the shim must NOT convert it into
    // a passkey ceremony — it calls through to the captured original instead.
    let script = passkey_bridge_script();
    // The publicKey guard precedes the passkey `enqueue`, and the else-branch
    // is a call-through to the original implementation.
    assert!(script.contains("if(options&&options.publicKey)return enqueue('get',options)"));
    assert!(script.contains("if(options&&options.publicKey)return enqueue('create',options)"));
    assert!(script.contains("if(origGet)return origGet(options)"));
    assert!(script.contains("if(origCreate)return origCreate(options)"));
    // The `enqueue` path (publicKey only) is the sole caller of the passkey
    // beacon queue, so a non-publicKey get can never reach it. Prove the
    // guard sits before the queue push, not after.
    let get_guard = script
        .find("if(options&&options.publicKey)return enqueue('get',options)")
        .expect("get guard present");
    let enqueue_impl = script
        .find("function enqueue(kind,options)")
        .expect("enqueue defined");
    assert!(
        enqueue_impl < get_guard,
        "enqueue is defined before the guarded call site"
    );

    // security-2(a): the ceremony only dispatches with transient activation,
    // and a gesture-less call is rejected rather than auto-signed.
    assert!(script.contains("if(ua&&typeof ua.isActive==='boolean')return ua.isActive"));
    assert!(script.contains("out.user_present=hasUserGesture()"));
    assert!(script.contains(
            "if(!item.user_present)return Promise.reject(new DOMException('Passkey ceremony requires a user gesture','NotAllowedError'))"
        ));
}

#[test]
fn webrtc_block_script_removes_the_reachable_webrtc_surface() {
    // `--disable-webrtc` (cef_init.rs) is confirmed non-functional (see
    // its doc comment); this opt-in renderer-level shim is the emergency block,
    // so pin exactly which JS-reachable entry points it removes when enabled.
    let script = webrtc_block_script();
    assert!(script.contains("delete w.RTCPeerConnection"));
    assert!(script.contains("delete w.webkitRTCPeerConnection"));
    assert!(script.contains("delete w.RTCDataChannel"));
    assert!(script.contains("delete w.RTCSessionDescription"));
    assert!(script.contains("delete w.RTCIceCandidate"));
    assert!(script.contains("w.MediaDevices.prototype.getUserMedia"));
    assert!(script.contains("w.MediaDevices.prototype.getDisplayMedia"));
    assert!(script.contains("w.navigator.mediaDevices.getUserMedia"));
    assert!(script.contains("w.navigator.mediaDevices.getDisplayMedia"));
    assert!(script.contains("delete w.navigator.getUserMedia"));
    assert!(script.contains("delete w.navigator.webkitGetUserMedia"));
    assert!(script.contains("delete w.navigator.mozGetUserMedia"));
    assert!(
        !script.contains("<script"),
        "webrtc block runs as a direct IIFE, not injected markup"
    );
    // Every deletion is individually try/catch-guarded so one already-gone
    // global (e.g. re-running after the page itself deleted something)
    // cannot abort the rest of the shim.
    assert_eq!(script.matches("catch(_e){}").count(), 14);
}

#[test]
fn webrtc_block_script_covers_subframes_not_just_the_main_frame() {
    // browser-3: a page's trivial bypass was a child iframe (its own
    // unpatched JS context). The shim now recurses through `frames` and
    // re-sweeps on DOM mutation, so a same-origin child/nested/late-inserted
    // iframe is stripped too — not only `get_main_frame`.
    let script = webrtc_block_script();
    // The strip logic is parameterised over a target window `w` and applied
    // to every reachable frame, rather than hard-coded to `window`.
    assert!(script.contains("function strip(w)"));
    assert!(script.contains("function sweep(w)"));
    // Recurse across child frames.
    assert!(script.contains("w.frames"));
    assert!(script.contains("sweep(cw)"));
    // Cover frames inserted after the first pass.
    assert!(script.contains("MutationObserver"));
    assert!(script.contains("childList:true,subtree:true"));
    // The entry point sweeps from the top window (which recurses down).
    assert!(script.contains("sweep(window)"));
}

#[test]
fn cef_webrtc_block_env_defaults_to_operational_surface() {
    assert!(
        !cef_webrtc_blocked_from_env_value(None),
        "CEF WebRTC should be available unless explicitly blocked"
    );
    for value in ["0", "false", "no", "off", "", "unexpected"] {
        assert!(
            !cef_webrtc_blocked_from_env_value(Some(value)),
            "{value:?} must not enable the legacy block"
        );
    }
    for value in ["1", "true", "TRUE", "yes", "on", " on "] {
        assert!(
            cef_webrtc_blocked_from_env_value(Some(value)),
            "{value:?} should enable the legacy block"
        );
    }
}

#[test]
fn idle_media_pump_backs_off_only_after_discovery_window() {
    // perf-6: awaiting the first paint is always active regardless of the
    // idle clock, so initial load latency is never regressed.
    assert_eq!(
        pump_interval(Duration::from_secs(30), true, false),
        PUMP_ACTIVE
    );
    // Recent activity and the bounded media-discovery window stay fast so CEF
    // can observe muted/silent autoplay before backing off.
    assert_eq!(pump_interval(Duration::ZERO, false, false), PUMP_ACTIVE);
    assert_eq!(pump_interval(SHIM_SETTLE / 2, false, false), PUMP_ACTIVE);
    // Active media stays fast even when the pointer is still and no new paint has
    // reached the shell yet; this prevents video advancing only on mouse motion.
    assert_eq!(
        pump_interval(Duration::from_secs(30), false, true),
        PUMP_ACTIVE
    );
    // Sustained quiet backs off so an idle tab stops spinning at 125 Hz.
    assert_eq!(pump_interval(SHIM_SETTLE, false, false), PUMP_IDLE);
    assert_eq!(
        pump_interval(Duration::from_secs(5), false, false),
        PUMP_IDLE
    );
    // The idle interval is a real, substantial back-off from the active spin.
    assert!(PUMP_IDLE >= PUMP_ACTIVE * 10);
}

#[test]
fn idle_media_pump_invalidation_tracks_discovery_window() {
    assert!(should_invalidate_view(media_discovery_active(
        Duration::from_secs(30),
        true,
        false
    )));
    assert!(should_invalidate_view(media_discovery_active(
        Duration::from_secs(30),
        false,
        true
    )));
    assert!(should_invalidate_view(media_discovery_active(
        SHIM_SETTLE / 2,
        false,
        false
    )));
    assert!(
        !should_invalidate_view(media_discovery_active(SHIM_SETTLE, false, false)),
        "settled non-media pages must be allowed to idle without paint nudges"
    );
}

#[test]
fn idle_media_pump_resize_nudge_is_bounded() {
    assert!(
        !should_resize_view(false, MEDIA_VIEW_RESIZE_NUDGE_INTERVAL),
        "settled non-media pages must not receive resize pulses"
    );
    assert!(
        !should_resize_view(true, MEDIA_VIEW_RESIZE_NUDGE_INTERVAL / 2),
        "resize pulses must be rate-limited"
    );
    assert!(should_resize_view(true, MEDIA_VIEW_RESIZE_NUDGE_INTERVAL));
    assert!(MEDIA_VIEW_RESIZE_NUDGE_INTERVAL >= PUMP_ACTIVE * 10);
}

#[test]
fn callback_media_state_keeps_the_pump_active_until_pause_or_navigation() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    assert!(!callbacks.state.active_media());

    callbacks
        .state
        .publish_media_metadata(r#"{"title":"Track","paused":false}"#.to_owned());
    assert!(callbacks.state.active_media());

    callbacks
        .state
        .publish_media_metadata(r#"{"title":"Track","paused":true}"#.to_owned());
    assert!(!callbacks.state.active_media());

    callbacks.state.publish_audio_state(true);
    assert!(callbacks.state.active_media());
    callbacks.state.record_navigation();
    assert!(!callbacks.state.active_media());
}

#[test]
fn shim_injector_injects_once_per_navigation_not_on_a_timer() {
    // browser-8: the shims re-inject when the navigation generation advances,
    // never on a bare wall-clock timer once the context is stable.
    let mut injector = ShimInjector::new();
    let t0 = Instant::now();
    // First sight of a generation always injects once.
    assert!(injector.should_inject(0, false, t0));
    // Same generation, stable (not settling): no re-injection even much later.
    assert!(!injector.should_inject(0, false, t0 + Duration::from_secs(10)));
    assert!(!injector.should_inject(0, false, t0 + Duration::from_secs(60)));
    // A real navigation (generation advanced) injects again.
    assert!(injector.should_inject(1, false, t0 + Duration::from_secs(61)));
    assert!(!injector.should_inject(1, false, t0 + Duration::from_secs(62)));
}

#[test]
fn shim_injector_reinjects_through_a_settling_document_then_stops() {
    // A freshly-navigated document that is still settling gets a bounded
    // re-inject (covers a slow commit under an ABI with no load callback),
    // but the cadence is throttled to SETTLE_INTERVAL, not every tick.
    let mut injector = ShimInjector::new();
    let t0 = Instant::now();
    // A new generation injects once.
    assert!(injector.should_inject(2, true, t0));
    // Immediately again while settling: throttled, not a per-tick spin.
    assert!(!injector.should_inject(2, true, t0 + Duration::from_millis(10)));
    assert!(!injector.should_inject(2, true, t0 + ShimInjector::SETTLE_INTERVAL / 2));
    // Once the settle interval elapses, re-inject to cover the commit.
    assert!(injector.should_inject(2, true, t0 + ShimInjector::SETTLE_INTERVAL));
    // When the same generation stops settling, injection stops entirely.
    assert!(!injector.should_inject(2, false, t0 + Duration::from_secs(30)));
}

#[test]
fn passkey_bridge_exposes_reusable_drain_and_heartbeat_is_lightweight() {
    // browser-8: the heavy bridge shim installs a reusable drain closure once
    // per context; the heartbeat script just invokes it instead of
    // recompiling the whole shim every 250 ms.
    let install = passkey_bridge_script();
    assert!(install.contains("window.__mdeBrowserPasskeyDrain=function"));
    assert!(install.contains("__mdeBrowserPasskeyQueue"));

    let drain = passkey_drain_script();
    assert!(drain.contains("window.__mdeBrowserPasskeyDrain"));
    // The heartbeat is only the invocation, not the shim: it must not carry
    // the credential-override installation that only needs to happen once.
    assert!(!drain.contains("creds.get=function"));
    assert!(!drain.contains("navigator.credentials"));
    // Materially smaller than the shim it replaces on the timer path.
    assert!(drain.len() * 4 < install.len());
}

#[test]
fn wait_for_readable_returns_promptly_when_the_fd_is_readable() {
    use std::io::Write;
    let (a, mut b) = UnixStream::pair().expect("socketpair");
    a.set_nonblocking(true).expect("nonblocking");
    b.write_all(b"x").expect("write");
    // A readable fd must not block for the full timeout: poll returns at once.
    let started = Instant::now();
    wait_for_readable(a.as_raw_fd(), Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn record_navigation_advances_the_generation_seen_by_the_pump() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    assert_eq!(callbacks.navigations(), 0);
    callbacks.state.record_navigation();
    callbacks.state.record_navigation();
    assert_eq!(callbacks.navigations(), 2);
}

#[test]
fn js_string_literal_escapes_script_sensitive_characters() {
    assert_eq!(js_string_literal("a\"b\\c\n\r\t"), r#""a\"b\\c\n\r\t""#);
    assert_eq!(js_string_literal("nul\u{1}"), r#""nul\u0001""#);
}

#[test]
fn callback_registry_returns_lifespan_and_render_peers() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let life = unsafe { get_life_span_handler(callbacks.client_ptr()) };
    let render = unsafe { get_render_handler(callbacks.client_ptr()) };
    let request = unsafe { get_request_handler(callbacks.client_ptr()) };
    assert!(!life.is_null());
    assert!(!render.is_null());
    assert!(!request.is_null());
    assert_ne!(life, render);
    assert_ne!(render, request);
}

#[test]
fn callbacks_record_created_and_view_paint() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let life = unsafe { get_life_span_handler(callbacks.client_ptr()) };
    let render = unsafe { get_render_handler(callbacks.client_ptr()) };
    unsafe { on_after_created(life, ptr::null_mut()) };
    let mut rect = CefRect {
        x: -1,
        y: -1,
        width: 0,
        height: 0,
    };
    unsafe { get_view_rect(render, ptr::null_mut(), &mut rect) };
    assert_eq!((rect.x, rect.y, rect.width, rect.height), (0, 0, 320, 200));
    let pixel = 0_u32;
    unsafe {
        on_paint(
            render,
            ptr::null_mut(),
            PET_VIEW,
            0,
            ptr::null(),
            (&pixel as *const u32).cast(),
            320,
            200,
        )
    };
    assert_eq!(callbacks.created(), 1);
    assert_eq!(callbacks.paints(), 1);
    assert_eq!(callbacks.last_paint_width(), 320);
    assert_eq!(callbacks.last_paint_height(), 200);
}

#[test]
fn resize_control_changes_the_next_view_rect() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let render = unsafe { get_render_handler(callbacks.client_ptr()) };
    callbacks.resize(640, 480);
    let mut rect = CefRect {
        x: -1,
        y: -1,
        width: 0,
        height: 0,
    };
    unsafe { get_view_rect(render, ptr::null_mut(), &mut rect) };
    assert_eq!((rect.x, rect.y, rect.width, rect.height), (0, 0, 640, 480));
}

#[test]
fn callbacks_publish_paint_to_the_bookmarks_frame_sink() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    assert_eq!(fds.len(), 1);
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::AttachFrame
    );

    let render = unsafe { get_render_handler(callbacks.client_ptr()) };
    unsafe {
        on_paint(
            render,
            ptr::null_mut(),
            PET_VIEW,
            0,
            ptr::null(),
            [0x7a_u8; 2 * 2 * 4].as_ptr().cast(),
            2,
            2,
        )
    };

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("paint recv") else {
        panic!("expected paint")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    let EventMsg::PaintReady { seq } = EventMsg::decode(&payload).expect("event") else {
        panic!("expected paint ready")
    };
    assert_eq!(seq % 2, 0);
    assert_eq!(callbacks.paints(), 1);
}

#[test]
fn pdf_completion_callback_publishes_helper_event() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");

    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let callback = callbacks.retain_pdf_callback();
    assert!(!callback.is_null(), "PDF callback retained");
    assert_eq!(callbacks.state.retained_pdf_callback_count(), 1);
    assert!(
        registry()
            .lock()
            .expect("registry")
            .contains_key(&(callback as usize)),
        "retained PDF callback is resolvable for CEF delivery"
    );
    let path = unique_test_pdf_path("cef-finished-ok");
    std::fs::write(&path, b"%PDF-1.7\n% test\n").expect("pdf fixture");
    let path_text = path.to_string_lossy().into_owned();
    let cef_path = CefStringOwned::new(&path_text).expect("pdf path");

    unsafe { on_pdf_print_finished(callback, cef_path.as_ptr(), 1) };

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("pdf recv") else {
        panic!("expected pdf event")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PdfSaved {
            path: path_text,
            ok: true,
        }
    );
    assert_eq!(
        callbacks.state.retained_pdf_callback_count(),
        1,
        "the PDF callback currently on CEF's stack is not freed inside its own callback"
    );
    assert_eq!(callbacks.state.finished_pdf_callback_count(), 1);
    assert!(
        !registry()
            .lock()
            .expect("registry")
            .contains_key(&(callback as usize)),
        "completed PDF callback no longer resolves through the registry"
    );

    let next_callback = callbacks.retain_pdf_callback();

    assert!(!next_callback.is_null(), "next PDF callback retained");
    assert_eq!(
        callbacks.state.retained_pdf_callback_count(),
        1,
        "finished PDF callbacks are purged before retaining another callback"
    );
    assert_eq!(callbacks.state.finished_pdf_callback_count(), 0);
    assert!(
        registry()
            .lock()
            .expect("registry")
            .contains_key(&(next_callback as usize)),
        "the next PDF callback remains registered"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn pdf_completion_callback_rejects_missing_pdf_output() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let callback = callbacks.retain_pdf_callback();
    assert!(!callback.is_null(), "PDF callback retained");
    let path = unique_test_pdf_path("cef-finished-missing");
    let path_text = path.to_string_lossy().into_owned();
    let cef_path = CefStringOwned::new(&path_text).expect("pdf path");

    unsafe { on_pdf_print_finished(callback, cef_path.as_ptr(), 1) };

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("pdf recv") else {
        panic!("expected pdf event")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PdfSaved {
            path: path_text,
            ok: false,
        }
    );
    assert_eq!(
        callbacks.state.finished_pdf_callback_count(),
        1,
        "failed PDF completions still finish the one-shot callback"
    );
    assert!(
        !registry()
            .lock()
            .expect("registry")
            .contains_key(&(callback as usize)),
        "failed PDF completion also removes the callback registry entry"
    );
}

#[test]
fn favicon_download_callbacks_are_removed_from_registry_and_purged_after_completion() {
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let first = callbacks.state.retain_download_image_callback();
    assert!(!first.is_null(), "favicon callback retained");
    assert_eq!(callbacks.state.retained_download_image_callback_count(), 1);
    assert!(
        registry()
            .lock()
            .expect("registry")
            .contains_key(&(first as usize)),
        "retained callback is resolvable for CEF delivery"
    );

    unsafe { on_download_image_finished(first, ptr::null(), 404, ptr::null_mut()) };

    assert_eq!(
        callbacks.state.retained_download_image_callback_count(),
        1,
        "the callback currently on CEF's stack is not freed inside its own callback"
    );
    assert_eq!(callbacks.state.finished_download_image_callback_count(), 1);
    assert!(
        !registry()
            .lock()
            .expect("registry")
            .contains_key(&(first as usize)),
        "completed one-shot callback no longer resolves through the registry"
    );

    let second = callbacks.state.retain_download_image_callback();

    assert!(!second.is_null(), "next favicon callback retained");
    assert_eq!(
        callbacks.state.retained_download_image_callback_count(),
        1,
        "finished callbacks are purged before retaining another favicon callback"
    );
    assert_eq!(callbacks.state.finished_download_image_callback_count(), 0);
    assert!(
        registry()
            .lock()
            .expect("registry")
            .contains_key(&(second as usize)),
        "the next callback remains registered"
    );
}

#[test]
fn favicon_png_binary_value_is_released_after_copy() {
    let binary = TestBinaryValue::new(vec![0x89, b'P', b'N', b'G']);
    let image = TestImage::new(binary.as_mut_ptr());

    assert_eq!(
        image_as_png(image.as_mut_ptr()),
        Some(vec![0x89, b'P', b'N', b'G'])
    );
    assert_eq!(
        binary.releases(),
        1,
        "cef_binary_value_t returned by get_as_png is released after copying"
    );
}

#[test]
fn request_handler_registry_returns_resource_request_peer() {
    let callbacks = CefBrowserCallbacks::new(
        320,
        200,
        None,
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let request = unsafe { get_request_handler(callbacks.client_ptr()) };
    let mut disable_default_handling = -1;
    let resource_request = unsafe {
        get_resource_request_handler(
            request,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            0,
            0,
            ptr::null(),
            &mut disable_default_handling,
        )
    };
    assert_eq!(disable_default_handling, 0);
    assert!(!resource_request.is_null());
}

#[test]
fn resource_verdict_transport_uses_async_cef_callback() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "https://www.google-analytics.com/collect".to_owned(),
        RESOURCE_OTHER,
        cef_callback.as_mut_ptr(),
    );
    assert_eq!(rv, RV_CONTINUE_ASYNC);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("resource request recv") else {
        panic!("expected resource request")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::ResourceRequest {
            id: 1,
            url: "https://www.google-analytics.com/collect".to_owned(),
            resource: RESOURCE_OTHER,
        }
    );

    callbacks.apply_resource_verdict(1, false);
    assert_eq!(cef_callback.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.continued.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn resource_verdict_pending_callbacks_are_bounded_and_fail_closed() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let mut held_callbacks = Vec::new();
    for idx in 0..MAX_PENDING_RESOURCE_REQUESTS {
        let callback = TestCefCallback::new();
        let id = (idx + 1) as u64;
        let url = format!("https://ads.example/{idx}.js");
        let rv = callbacks.state.begin_resource_request(
            url.clone(),
            RESOURCE_OTHER,
            callback.as_mut_ptr(),
        );
        assert_eq!(rv, RV_CONTINUE_ASYNC);
        assert_eq!(callback.add_refs.load(Ordering::SeqCst), 1);

        let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("resource request recv") else {
            panic!("expected resource request")
        };
        assert!(fds.is_empty());
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        assert_eq!(
            EventMsg::decode(&payload).expect("event"),
            EventMsg::ResourceRequest {
                id,
                url,
                resource: RESOURCE_OTHER,
            }
        );
        held_callbacks.push(callback);
    }
    assert_eq!(
        callbacks.state.pending_resource_request_count(),
        MAX_PENDING_RESOURCE_REQUESTS
    );

    let overflow = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "https://ads.example/overflow.js".to_owned(),
        RESOURCE_OTHER,
        overflow.as_mut_ptr(),
    );

    assert_eq!(rv, RV_CANCEL);
    assert_eq!(
        callbacks.state.pending_resource_request_count(),
        MAX_PENDING_RESOURCE_REQUESTS,
        "overflow must not grow the held CEF callback set"
    );
    assert_eq!(
        overflow.add_refs.load(Ordering::SeqCst),
        0,
        "overflow is canceled synchronously without taking a stash ref"
    );
    assert_eq!(overflow.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(overflow.releases.load(Ordering::SeqCst), 0);

    callbacks.apply_resource_verdict(1, true);
    assert_eq!(held_callbacks[0].continued.load(Ordering::SeqCst), 1);
    assert_eq!(held_callbacks[0].releases.load(Ordering::SeqCst), 1);
    assert_eq!(
        callbacks.state.pending_resource_request_count(),
        MAX_PENDING_RESOURCE_REQUESTS - 1
    );

    let recovered = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "https://ads.example/recovered.js".to_owned(),
        RESOURCE_OTHER,
        recovered.as_mut_ptr(),
    );
    assert_eq!(rv, RV_CONTINUE_ASYNC, "freeing a slot allows new requests");
    assert_eq!(recovered.add_refs.load(Ordering::SeqCst), 1);

    callbacks.state.cancel_pending_resource_requests();
    assert_eq!(
        callbacks.state.pending_resource_request_count(),
        0,
        "test drains pending callbacks before dropping fake CEF callback blocks"
    );
}

#[test]
fn page_text_beacon_is_intercepted_and_published_without_adfilter_roundtrip() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "mde-page-text://capture/77?text=hello%20page".to_owned(),
        RESOURCE_OTHER,
        cef_callback.as_mut_ptr(),
    );
    assert_eq!(rv, RV_CANCEL);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("page text recv") else {
        panic!("expected page text")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PageText {
            id: 77,
            text: "hello page".to_owned(),
        }
    );
    assert_eq!(cef_callback.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.continued.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 0);
}

#[test]
fn media_metadata_beacon_is_intercepted_and_published_without_adfilter_roundtrip() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "https://mde-media.invalid/metadata/?body=%7B%22title%22%3A%22Track%22%2C%22artist%22%3A%22Artist%22%7D".to_owned(),
        RESOURCE_OTHER,
        cef_callback.as_mut_ptr(),
    );
    assert_eq!(rv, RV_CANCEL);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("media metadata recv") else {
        panic!("expected media metadata")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::MediaMetadata {
            body: r#"{"title":"Track","artist":"Artist"}"#.to_owned(),
        }
    );
    assert_eq!(cef_callback.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.continued.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 0);
}

#[test]
fn native_page_text_visitor_publishes_and_reclaims_callback() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let visitor = callbacks.state.retain_page_text_visitor(91, 8);
    assert!(!visitor.is_null());
    assert_eq!(callbacks.state.retained_page_text_visitor_count(), 1);
    assert_eq!(callbacks.state.finished_page_text_visitor_count(), 0);

    let text = CefStringOwned::new("hello native").expect("text");
    unsafe { on_page_text_visited(visitor, text.as_ptr().cast::<CefString>()) };

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("page text recv") else {
        panic!("expected page text")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PageText {
            id: 91,
            text: "hello na".to_owned(),
        }
    );
    assert_eq!(callbacks.state.retained_page_text_visitor_count(), 1);
    assert_eq!(callbacks.state.finished_page_text_visitor_count(), 1);

    callbacks.state.purge_finished_page_text_visitors(None);
    assert_eq!(callbacks.state.retained_page_text_visitor_count(), 0);
    assert_eq!(callbacks.state.finished_page_text_visitor_count(), 0);
}

#[test]
fn page_scrape_beacon_is_intercepted_and_published_without_adfilter_roundtrip() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
            "https://mde-page-scrape.invalid/capture/88?body=%7B%22text%22%3A%22hello%22%2C%22links%22%3A%5B%5D%7D".to_owned(),
            RESOURCE_OTHER,
            cef_callback.as_mut_ptr(),
        );
    assert_eq!(rv, RV_CANCEL);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("page scrape recv") else {
        panic!("expected page scrape")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PageScrape {
            id: 88,
            body: r#"{"text":"hello","links":[]}"#.to_owned(),
        }
    );
    assert_eq!(cef_callback.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.continued.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 0);
}

#[test]
fn page_text_dialog_beacon_is_intercepted_without_user_jsdialog_event() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let (on_jsdialog, handler) = resolve_on_jsdialog(&callbacks);
    let origin = CefStringOwned::new("https://doc.example/").expect("origin");
    let message = CefStringOwned::new("https://mde-page-text.invalid/capture/77?text=hello%20page")
        .expect("message");
    let cef_callback = TestJsDialogCallback::new();
    let mut suppress = -1;

    let rv = unsafe {
        on_jsdialog(
            handler,
            ptr::null_mut(),
            origin.as_ptr().cast::<CefString>(),
            0,
            message.as_ptr().cast::<CefString>(),
            ptr::null(),
            cef_callback.as_mut_ptr(),
            &mut suppress,
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(suppress, 0);
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.success.load(Ordering::SeqCst), 1);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("page text recv") else {
        panic!("expected page text")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PageText {
            id: 77,
            text: "hello page".to_owned(),
        }
    );
    shell.set_nonblocking(true).expect("nonblocking shell");
    assert!(
        matches!(
            recv(&shell).expect("no jsdialog event"),
            RecvOutcome::WouldBlock
        ),
        "internal page-text beacons must not surface as user JS dialogs"
    );
}

#[test]
fn page_scrape_dialog_beacon_is_intercepted_without_user_jsdialog_event() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let (on_jsdialog, handler) = resolve_on_jsdialog(&callbacks);
    let origin = CefStringOwned::new("https://doc.example/").expect("origin");
    let message = CefStringOwned::new(
        "https://mde-page-scrape.invalid/capture/88?body=%7B%22text%22%3A%22hello%22%7D",
    )
    .expect("message");
    let cef_callback = TestJsDialogCallback::new();
    let mut suppress = -1;

    let rv = unsafe {
        on_jsdialog(
            handler,
            ptr::null_mut(),
            origin.as_ptr().cast::<CefString>(),
            0,
            message.as_ptr().cast::<CefString>(),
            ptr::null(),
            cef_callback.as_mut_ptr(),
            &mut suppress,
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(suppress, 0);
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.success.load(Ordering::SeqCst), 1);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("page scrape recv") else {
        panic!("expected page scrape")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PageScrape {
            id: 88,
            body: r#"{"text":"hello"}"#.to_owned(),
        }
    );
    shell.set_nonblocking(true).expect("nonblocking shell");
    assert!(
        matches!(
            recv(&shell).expect("no jsdialog event"),
            RecvOutcome::WouldBlock
        ),
        "internal page-scrape beacons must not surface as user JS dialogs"
    );
}

#[test]
fn user_jsdialog_still_publishes_and_auto_resolves() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let (on_jsdialog, handler) = resolve_on_jsdialog(&callbacks);
    let origin = CefStringOwned::new("https://doc.example/").expect("origin");
    let message = CefStringOwned::new("real alert").expect("message");
    let cef_callback = TestJsDialogCallback::new();
    let mut suppress = -1;

    let rv = unsafe {
        on_jsdialog(
            handler,
            ptr::null_mut(),
            origin.as_ptr().cast::<CefString>(),
            0,
            message.as_ptr().cast::<CefString>(),
            ptr::null(),
            cef_callback.as_mut_ptr(),
            &mut suppress,
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(suppress, 0);
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.success.load(Ordering::SeqCst), 1);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("jsdialog recv") else {
        panic!("expected jsdialog")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::JsDialog {
            kind: 0,
            message: "real alert".to_owned(),
            origin: "https://doc.example/".to_owned(),
        }
    );
}

#[test]
fn passkey_beacon_is_intercepted_and_published_without_adfilter_roundtrip() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "https://mde-passkey.invalid/request/?body=%7B%22ceremony%22%3A%22get%22%7D".to_owned(),
        RESOURCE_OTHER,
        cef_callback.as_mut_ptr(),
    );
    assert_eq!(rv, RV_CANCEL);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("passkey recv") else {
        panic!("expected passkey event")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PasskeyRequest {
            body: r#"{"ceremony":"get"}"#.to_owned(),
        }
    );
    assert_eq!(cef_callback.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.continued.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 0);
}

#[test]
fn text_probe_status_and_event_drain_are_operator_readable() {
    let probe = CefTextProbe {
        browser: CefBrowserProbe {
            url: "https://example.test/".to_owned(),
            width: 1024,
            height: 768,
            created: 1,
            paints: 3,
            last_paint_width: 1024,
            last_paint_height: 768,
        },
        expected: "mde-extension-smoke-ok".to_owned(),
        text_bytes: 128,
    };
    let line = probe.status_line();
    assert!(line.contains("CEF_TEXT_PROBE_READY"));
    assert!(line.contains("https://example.test/"));
    assert!(line.contains("marker_bytes=22"));
    assert!(line.contains("text_bytes=128"));

    let mut bytes = wire::frame(
        &EventMsg::PageText {
            id: 99,
            text: "ignored".to_owned(),
        }
        .encode(),
    );
    bytes.extend(wire::frame(
        &EventMsg::PageText {
            id: 42,
            text: "mde-extension-smoke-ok".to_owned(),
        }
        .encode(),
    ));
    assert_eq!(
        drain_page_text_events(&mut bytes, 42),
        Some("mde-extension-smoke-ok".to_owned())
    );
    assert!(bytes.is_empty());

    let err = CefBrowserError::TextProbeMissing {
        created: 1,
        paints: 2,
        text_bytes: 12,
    }
    .to_string();
    assert!(err.contains("CEF text marker"));
    assert!(err.contains("text_bytes=12"));
}

#[test]
fn browser_probe_status_line_is_operator_readable() {
    let line = CefBrowserProbe {
        url: "https://example.com/".to_owned(),
        width: 800,
        height: 600,
        created: 1,
        paints: 2,
        last_paint_width: 800,
        last_paint_height: 600,
    }
    .status_line();
    assert!(line.contains("CEF_BROWSER_PAINT_READY"));
    assert!(line.contains("https://example.com/"));
    assert!(line.contains("last_paint=800x600"));
}

fn read_usize(bytes: &[u8], offset: usize) -> usize {
    let mut data = [0; std::mem::size_of::<usize>()];
    data.copy_from_slice(&bytes[offset..offset + std::mem::size_of::<usize>()]);
    usize::from_ne_bytes(data)
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    let mut data = [0; std::mem::size_of::<i32>()];
    data.copy_from_slice(&bytes[offset..offset + std::mem::size_of::<i32>()]);
    i32::from_ne_bytes(data)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut data = [0; std::mem::size_of::<u32>()];
    data.copy_from_slice(&bytes[offset..offset + std::mem::size_of::<u32>()]);
    u32::from_ne_bytes(data)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    let mut data = [0; std::mem::size_of::<u16>()];
    data.copy_from_slice(&bytes[offset..offset + std::mem::size_of::<u16>()]);
    u16::from_ne_bytes(data)
}

unsafe extern "C" fn noop_userfree_free(_string: *mut c_void) {}

/// Empty string-list stub: reports zero elements, so `request_favicon` no-ops.
unsafe extern "C" fn noop_string_list_size(_list: *mut c_void) -> usize {
    0
}

/// Never-called string-list value stub (the size stub reports an empty list).
unsafe extern "C" fn noop_string_list_value(
    _list: *mut c_void,
    _index: usize,
    _value: *mut c_void,
) -> c_int {
    0
}

const TEST_CEF_BINARY_VALUE_SIZE: usize = CEF_BINARY_VALUE_GET_DATA_OFFSET + size_of::<usize>();
const TEST_CEF_IMAGE_SIZE: usize = CEF_IMAGE_GET_AS_PNG_OFFSET + size_of::<usize>();

struct TestBinaryValue {
    block: CefCallbackBlock<TEST_CEF_BINARY_VALUE_SIZE>,
    bytes: Box<[u8]>,
    releases: Box<AtomicUsize>,
}

impl TestBinaryValue {
    fn new(bytes: Vec<u8>) -> Box<Self> {
        let releases = Box::new(AtomicUsize::new(0));
        let mut block = CefCallbackBlock::new(TEST_CEF_BINARY_VALUE_SIZE);
        block.put_fn(
            BASE_RELEASE_OFFSET,
            fn_ptr(test_binary_release as *const ()),
        );
        block.put_fn(
            CEF_BINARY_VALUE_GET_SIZE_OFFSET,
            fn_ptr(test_binary_get_size as *const ()),
        );
        block.put_fn(
            CEF_BINARY_VALUE_GET_DATA_OFFSET,
            fn_ptr(test_binary_get_data as *const ()),
        );
        let value = Box::new(Self {
            block,
            bytes: bytes.into_boxed_slice(),
            releases,
        });
        value.install_state_pointer();
        value
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.block.as_mut_ptr()
    }

    fn releases(&self) -> usize {
        self.releases.load(Ordering::SeqCst)
    }

    fn install_state_pointer(&self) {
        let state = TestBinaryValueState {
            bytes: self.bytes.as_ptr() as usize,
            len: self.bytes.len(),
            releases: self.releases.as_ref() as *const AtomicUsize as usize,
        };
        test_binary_registry()
            .lock()
            .expect("test binary registry")
            .insert(self.as_mut_ptr() as usize, state);
    }
}

impl Drop for TestBinaryValue {
    fn drop(&mut self) {
        let _ = test_binary_registry()
            .lock()
            .map(|mut registry| registry.remove(&(self.as_mut_ptr() as usize)));
    }
}

#[derive(Clone, Copy)]
struct TestBinaryValueState {
    bytes: usize,
    len: usize,
    releases: usize,
}

fn test_binary_registry() -> &'static Mutex<HashMap<usize, TestBinaryValueState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, TestBinaryValueState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe extern "C" fn test_binary_get_size(self_: *mut c_void) -> usize {
    test_binary_state(self_).len
}

unsafe extern "C" fn test_binary_get_data(
    self_: *mut c_void,
    buffer: *mut c_void,
    buffer_size: usize,
    data_offset: usize,
) -> usize {
    if buffer.is_null() {
        return 0;
    }
    let state = test_binary_state(self_);
    let offset = data_offset.min(state.len);
    let len = buffer_size.min(state.len.saturating_sub(offset));
    if len == 0 {
        return 0;
    }
    // SAFETY: the registry stores a pointer to the boxed test bytes, which outlive
    // this callback; `buffer` points to the caller's writable `len`-byte output.
    unsafe {
        ptr::copy_nonoverlapping((state.bytes as *const u8).add(offset), buffer.cast(), len);
    }
    len
}

unsafe extern "C" fn test_binary_release(self_: *mut c_void) -> c_int {
    let state = test_binary_state(self_);
    // SAFETY: `releases` points to the boxed counter owned by `TestBinaryValue`.
    unsafe { (*(state.releases as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    0
}

fn test_binary_state(self_: *mut c_void) -> TestBinaryValueState {
    *test_binary_registry()
        .lock()
        .expect("test binary registry")
        .get(&(self_ as usize))
        .expect("registered test binary value")
}

struct TestImage {
    block: CefCallbackBlock<TEST_CEF_IMAGE_SIZE>,
    binary: *mut c_void,
}

impl TestImage {
    fn new(binary: *mut c_void) -> Box<Self> {
        let mut block = CefCallbackBlock::new(TEST_CEF_IMAGE_SIZE);
        block.put_fn(
            CEF_IMAGE_GET_AS_PNG_OFFSET,
            fn_ptr(test_image_get_as_png as *const ()),
        );
        let value = Box::new(Self { block, binary });
        value.install_state_pointer();
        value
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.block.as_mut_ptr()
    }

    fn install_state_pointer(&self) {
        test_image_registry()
            .lock()
            .expect("test image registry")
            .insert(self.as_mut_ptr() as usize, self.binary as usize);
    }
}

impl Drop for TestImage {
    fn drop(&mut self) {
        let _ = test_image_registry()
            .lock()
            .map(|mut registry| registry.remove(&(self.as_mut_ptr() as usize)));
    }
}

fn test_image_registry() -> &'static Mutex<HashMap<usize, usize>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe extern "C" fn test_image_get_as_png(
    self_: *mut c_void,
    _scale_factor: f32,
    _with_transparency: c_int,
    pixel_width: *mut c_int,
    pixel_height: *mut c_int,
) -> *mut c_void {
    if !pixel_width.is_null() {
        // SAFETY: CEF-style out-param supplied by `image_as_png`.
        unsafe { *pixel_width = 16 };
    }
    if !pixel_height.is_null() {
        // SAFETY: CEF-style out-param supplied by `image_as_png`.
        unsafe { *pixel_height = 16 };
    }
    test_image_registry()
        .lock()
        .expect("test image registry")
        .get(&(self_ as usize))
        .copied()
        .unwrap_or(0) as *mut c_void
}

struct TestCefCallback {
    block: CefCallbackBlock<CEF_CALLBACK_SIZE>,
    add_refs: Box<AtomicUsize>,
    releases: Box<AtomicUsize>,
    continued: Box<AtomicUsize>,
    cancelled: Box<AtomicUsize>,
}

impl TestCefCallback {
    fn new() -> Box<Self> {
        let add_refs = Box::new(AtomicUsize::new(0));
        let releases = Box::new(AtomicUsize::new(0));
        let continued = Box::new(AtomicUsize::new(0));
        let cancelled = Box::new(AtomicUsize::new(0));
        let mut block = CefCallbackBlock::new(CEF_CALLBACK_SIZE);
        block.put_fn(BASE_ADD_REF_OFFSET, fn_ptr(test_add_ref as *const ()));
        block.put_fn(BASE_RELEASE_OFFSET, fn_ptr(test_release as *const ()));
        block.put_fn(CEF_CALLBACK_CONT_OFFSET, fn_ptr(test_cont as *const ()));
        block.put_fn(CEF_CALLBACK_CANCEL_OFFSET, fn_ptr(test_cancel as *const ()));
        let value = Box::new(Self {
            block,
            add_refs,
            releases,
            continued,
            cancelled,
        });
        value.install_state_pointers();
        value
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.block.as_mut_ptr()
    }

    fn install_state_pointers(&self) {
        let state = TestCefCallbackState {
            add_refs: self.add_refs.as_ref() as *const AtomicUsize as usize,
            releases: self.releases.as_ref() as *const AtomicUsize as usize,
            continued: self.continued.as_ref() as *const AtomicUsize as usize,
            cancelled: self.cancelled.as_ref() as *const AtomicUsize as usize,
        };
        test_callback_registry()
            .lock()
            .expect("test callback registry")
            .insert(self.as_mut_ptr() as usize, state);
    }
}

impl Drop for TestCefCallback {
    fn drop(&mut self) {
        let _ = test_callback_registry()
            .lock()
            .map(|mut registry| registry.remove(&(self.as_mut_ptr() as usize)));
    }
}

#[derive(Clone, Copy)]
struct TestCefCallbackState {
    add_refs: usize,
    releases: usize,
    continued: usize,
    cancelled: usize,
}

fn test_callback_registry() -> &'static Mutex<HashMap<usize, TestCefCallbackState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, TestCefCallbackState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe extern "C" fn test_add_ref(self_: *mut c_void) {
    test_counter(self_, |state| state.add_refs).fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn test_release(self_: *mut c_void) -> c_int {
    test_counter(self_, |state| state.releases).fetch_add(1, Ordering::SeqCst);
    0
}

unsafe extern "C" fn test_cont(self_: *mut c_void) {
    test_counter(self_, |state| state.continued).fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn test_cancel(self_: *mut c_void) {
    test_counter(self_, |state| state.cancelled).fetch_add(1, Ordering::SeqCst);
}

fn test_counter(
    self_: *mut c_void,
    select: impl FnOnce(TestCefCallbackState) -> usize,
) -> &'static AtomicUsize {
    let state = test_callback_registry()
        .lock()
        .expect("test callback registry")
        .get(&(self_ as usize))
        .copied()
        .expect("registered test callback");
    let ptr = select(state);
    unsafe { &*(ptr as *const AtomicUsize) }
}

/// A fake `cef_jsdialog_callback_t` recording add_ref/release counts and the
/// `cont(success, user_input)` argument. Used by beforeunload tests without live
/// CEF or a native dialog.
struct TestJsDialogCallback {
    block: CefCallbackBlock<CEF_CALLBACK_SIZE>,
    add_refs: Box<AtomicUsize>,
    releases: Box<AtomicUsize>,
    conts: Box<AtomicUsize>,
    success: Box<AtomicI32>,
}

impl TestJsDialogCallback {
    fn new() -> Box<Self> {
        let add_refs = Box::new(AtomicUsize::new(0));
        let releases = Box::new(AtomicUsize::new(0));
        let conts = Box::new(AtomicUsize::new(0));
        let success = Box::new(AtomicI32::new(-1));
        let mut block = CefCallbackBlock::new(CEF_CALLBACK_SIZE);
        block.put_fn(
            BASE_ADD_REF_OFFSET,
            fn_ptr(test_jsdialog_add_ref as *const ()),
        );
        block.put_fn(
            BASE_RELEASE_OFFSET,
            fn_ptr(test_jsdialog_release as *const ()),
        );
        block.put_fn(
            CEF_CALLBACK_CONT_OFFSET,
            fn_ptr(test_jsdialog_cont as *const ()),
        );
        let value = Box::new(Self {
            block,
            add_refs,
            releases,
            conts,
            success,
        });
        value.install_state_pointers();
        value
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.block.as_mut_ptr()
    }

    fn install_state_pointers(&self) {
        let state = TestJsDialogCallbackState {
            add_refs: self.add_refs.as_ref() as *const AtomicUsize as usize,
            releases: self.releases.as_ref() as *const AtomicUsize as usize,
            conts: self.conts.as_ref() as *const AtomicUsize as usize,
            success: self.success.as_ref() as *const AtomicI32 as usize,
        };
        test_jsdialog_registry()
            .lock()
            .expect("test jsdialog registry")
            .insert(self.as_mut_ptr() as usize, state);
    }
}

impl Drop for TestJsDialogCallback {
    fn drop(&mut self) {
        let _ = test_jsdialog_registry()
            .lock()
            .map(|mut registry| registry.remove(&(self.as_mut_ptr() as usize)));
    }
}

#[derive(Clone, Copy)]
struct TestJsDialogCallbackState {
    add_refs: usize,
    releases: usize,
    conts: usize,
    success: usize,
}

fn test_jsdialog_registry() -> &'static Mutex<HashMap<usize, TestJsDialogCallbackState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, TestJsDialogCallbackState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn test_jsdialog_state(self_: *mut c_void) -> TestJsDialogCallbackState {
    test_jsdialog_registry()
        .lock()
        .expect("test jsdialog registry")
        .get(&(self_ as usize))
        .copied()
        .expect("registered test jsdialog callback")
}

unsafe extern "C" fn test_jsdialog_add_ref(self_: *mut c_void) {
    let state = test_jsdialog_state(self_);
    // SAFETY: the recorder outlives the callback (owned by TestJsDialogCallback).
    unsafe { (*(state.add_refs as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
}

unsafe extern "C" fn test_jsdialog_release(self_: *mut c_void) -> c_int {
    let state = test_jsdialog_state(self_);
    // SAFETY: as above.
    unsafe { (*(state.releases as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    0
}

unsafe extern "C" fn test_jsdialog_cont(
    self_: *mut c_void,
    success: c_int,
    _user_input: *const CefString,
) {
    let state = test_jsdialog_state(self_);
    // SAFETY: as above.
    unsafe {
        (*(state.conts as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
        (*(state.success as *const AtomicI32)).store(success, Ordering::SeqCst);
    }
}

/// `cef_jsdialog_handler_t::on_jsdialog` C signature.
type OnJsDialogFn = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    *const CefString,
    c_int,
    *const CefString,
    *const CefString,
    *mut c_void,
    *mut c_int,
) -> c_int;

/// `cef_jsdialog_handler_t::on_before_unload_dialog` C signature.
type OnBeforeUnloadFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *const CefString, c_int, *mut c_void) -> c_int;

/// Resolve `on_jsdialog` through the client vtable's `get_jsdialog_handler`
/// slot, then read the method from the installed handler.
fn resolve_on_jsdialog(callbacks: &CefBrowserCallbacks) -> (OnJsDialogFn, *mut c_void) {
    let handler = unsafe { get_jsdialog_handler(callbacks.client_ptr()) };
    assert!(!handler.is_null(), "get_jsdialog_handler returned null");
    let on_jsdialog: OnJsDialogFn = unsafe {
        std::mem::transmute(
            read_fn(handler, CEF_JSDIALOG_HANDLER_ON_JSDIALOG_OFFSET).expect("on_jsdialog slot"),
        )
    };
    (on_jsdialog, handler)
}

/// Resolve `on_before_unload_dialog` through the client vtable's
/// `get_jsdialog_handler` slot, then read the method from the installed handler.
fn resolve_on_before_unload(callbacks: &CefBrowserCallbacks) -> (OnBeforeUnloadFn, *mut c_void) {
    let handler = unsafe { get_jsdialog_handler(callbacks.client_ptr()) };
    assert!(!handler.is_null(), "get_jsdialog_handler returned null");
    let on_before_unload: OnBeforeUnloadFn = unsafe {
        std::mem::transmute(
            read_fn(handler, CEF_JSDIALOG_HANDLER_ON_BEFORE_UNLOAD_DIALOG_OFFSET)
                .expect("on_before_unload_dialog slot"),
        )
    };
    (on_before_unload, handler)
}

/// A fake `cef_permission_prompt_callback_t` recording add_ref/release counts and
/// the `cont(result)` argument, so the permission-grant path is asserted without
/// live CEF. Mirrors `TestCefCallback`, but its `cont` carries the
/// `cef_permission_request_result_t` int we need to observe (ACCEPT vs DENY).
struct TestPermissionCallback {
    block: CefCallbackBlock<CEF_PERMISSION_PROMPT_CALLBACK_SIZE>,
    add_refs: Box<AtomicUsize>,
    releases: Box<AtomicUsize>,
    conts: Box<AtomicUsize>,
    result: Box<AtomicI32>,
}

impl TestPermissionCallback {
    fn new() -> Box<Self> {
        let add_refs = Box::new(AtomicUsize::new(0));
        let releases = Box::new(AtomicUsize::new(0));
        let conts = Box::new(AtomicUsize::new(0));
        let result = Box::new(AtomicI32::new(-1));
        let mut block = CefCallbackBlock::new(CEF_PERMISSION_PROMPT_CALLBACK_SIZE);
        block.put_fn(BASE_ADD_REF_OFFSET, fn_ptr(test_perm_add_ref as *const ()));
        block.put_fn(BASE_RELEASE_OFFSET, fn_ptr(test_perm_release as *const ()));
        block.put_fn(
            CEF_PERMISSION_PROMPT_CALLBACK_CONT_OFFSET,
            fn_ptr(test_perm_cont as *const ()),
        );
        let value = Box::new(Self {
            block,
            add_refs,
            releases,
            conts,
            result,
        });
        value.install_state_pointers();
        value
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.block.as_mut_ptr()
    }

    fn install_state_pointers(&self) {
        let state = TestPermissionCallbackState {
            add_refs: self.add_refs.as_ref() as *const AtomicUsize as usize,
            releases: self.releases.as_ref() as *const AtomicUsize as usize,
            conts: self.conts.as_ref() as *const AtomicUsize as usize,
            result: self.result.as_ref() as *const AtomicI32 as usize,
        };
        test_permission_registry()
            .lock()
            .expect("test permission registry")
            .insert(self.as_mut_ptr() as usize, state);
    }
}

impl Drop for TestPermissionCallback {
    fn drop(&mut self) {
        let _ = test_permission_registry()
            .lock()
            .map(|mut registry| registry.remove(&(self.as_mut_ptr() as usize)));
    }
}

#[derive(Clone, Copy)]
struct TestPermissionCallbackState {
    add_refs: usize,
    releases: usize,
    conts: usize,
    result: usize,
}

fn test_permission_registry() -> &'static Mutex<HashMap<usize, TestPermissionCallbackState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, TestPermissionCallbackState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn test_permission_state(self_: *mut c_void) -> TestPermissionCallbackState {
    test_permission_registry()
        .lock()
        .expect("test permission registry")
        .get(&(self_ as usize))
        .copied()
        .expect("registered test permission callback")
}

unsafe extern "C" fn test_perm_add_ref(self_: *mut c_void) {
    let state = test_permission_state(self_);
    // SAFETY: the recorder outlives the callback (owned by TestPermissionCallback).
    unsafe { (*(state.add_refs as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
}

unsafe extern "C" fn test_perm_release(self_: *mut c_void) -> c_int {
    let state = test_permission_state(self_);
    // SAFETY: as above.
    unsafe { (*(state.releases as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    0
}

unsafe extern "C" fn test_perm_cont(self_: *mut c_void, result: c_int) {
    let state = test_permission_state(self_);
    // SAFETY: as above.
    unsafe {
        (*(state.conts as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
        (*(state.result as *const AtomicI32)).store(result, Ordering::SeqCst);
    }
}

/// A fake `cef_media_access_callback_t` recording add_ref/release counts and the
/// `cont(allowed_permissions)` bitmask returned to CEF.
struct TestMediaAccessCallback {
    block: CefCallbackBlock<CEF_MEDIA_ACCESS_CALLBACK_SIZE>,
    add_refs: Box<AtomicUsize>,
    releases: Box<AtomicUsize>,
    conts: Box<AtomicUsize>,
    allowed: Box<AtomicU32>,
}

impl TestMediaAccessCallback {
    fn new() -> Box<Self> {
        let add_refs = Box::new(AtomicUsize::new(0));
        let releases = Box::new(AtomicUsize::new(0));
        let conts = Box::new(AtomicUsize::new(0));
        let allowed = Box::new(AtomicU32::new(u32::MAX));
        let mut block = CefCallbackBlock::new(CEF_MEDIA_ACCESS_CALLBACK_SIZE);
        block.put_fn(BASE_ADD_REF_OFFSET, fn_ptr(test_media_add_ref as *const ()));
        block.put_fn(BASE_RELEASE_OFFSET, fn_ptr(test_media_release as *const ()));
        block.put_fn(
            CEF_MEDIA_ACCESS_CALLBACK_CONT_OFFSET,
            fn_ptr(test_media_cont as *const ()),
        );
        let value = Box::new(Self {
            block,
            add_refs,
            releases,
            conts,
            allowed,
        });
        value.install_state_pointers();
        value
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.block.as_mut_ptr()
    }

    fn install_state_pointers(&self) {
        let state = TestMediaAccessCallbackState {
            add_refs: self.add_refs.as_ref() as *const AtomicUsize as usize,
            releases: self.releases.as_ref() as *const AtomicUsize as usize,
            conts: self.conts.as_ref() as *const AtomicUsize as usize,
            allowed: self.allowed.as_ref() as *const AtomicU32 as usize,
        };
        test_media_registry()
            .lock()
            .expect("test media registry")
            .insert(self.as_mut_ptr() as usize, state);
    }
}

impl Drop for TestMediaAccessCallback {
    fn drop(&mut self) {
        let _ = test_media_registry()
            .lock()
            .map(|mut registry| registry.remove(&(self.as_mut_ptr() as usize)));
    }
}

#[derive(Clone, Copy)]
struct TestMediaAccessCallbackState {
    add_refs: usize,
    releases: usize,
    conts: usize,
    allowed: usize,
}

fn test_media_registry() -> &'static Mutex<HashMap<usize, TestMediaAccessCallbackState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, TestMediaAccessCallbackState>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn test_media_state(self_: *mut c_void) -> TestMediaAccessCallbackState {
    test_media_registry()
        .lock()
        .expect("test media registry")
        .get(&(self_ as usize))
        .copied()
        .expect("registered test media callback")
}

unsafe extern "C" fn test_media_add_ref(self_: *mut c_void) {
    let state = test_media_state(self_);
    // SAFETY: the recorder outlives the callback (owned by TestMediaAccessCallback).
    unsafe { (*(state.add_refs as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
}

unsafe extern "C" fn test_media_release(self_: *mut c_void) -> c_int {
    let state = test_media_state(self_);
    // SAFETY: as above.
    unsafe { (*(state.releases as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    0
}

unsafe extern "C" fn test_media_cont(self_: *mut c_void, allowed_permissions: u32) {
    let state = test_media_state(self_);
    // SAFETY: as above.
    unsafe {
        (*(state.conts as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
        (*(state.allowed as *const AtomicU32)).store(allowed_permissions, Ordering::SeqCst);
    }
}

/// `cef_permission_handler_t::on_show_permission_prompt` C signature.
type OnShowPromptFn = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    u64,
    *const CefString,
    u32,
    *mut c_void,
) -> c_int;

/// `cef_permission_handler_t::on_request_media_access_permission` C signature.
type OnRequestMediaAccessFn = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *const CefString,
    u32,
    *mut c_void,
) -> c_int;

/// Resolve `on_show_permission_prompt` through the client vtable's
/// `get_permission_handler` slot (proving the getter landed at offset 120), then
/// read the method straight from its installed vtable offset.
fn resolve_on_show_permission_prompt(
    callbacks: &CefBrowserCallbacks,
) -> (OnShowPromptFn, *mut c_void) {
    let handler = unsafe { get_permission_handler(callbacks.client_ptr()) };
    assert!(!handler.is_null(), "get_permission_handler returned null");
    let on_show: OnShowPromptFn = unsafe {
        std::mem::transmute(
            read_fn(handler, CEF_PERMISSION_HANDLER_ON_SHOW_PROMPT_OFFSET)
                .expect("on_show_permission_prompt slot"),
        )
    };
    (on_show, handler)
}

/// Resolve `on_request_media_access_permission` through the installed permission
/// handler vtable.
fn resolve_on_request_media_access(
    callbacks: &CefBrowserCallbacks,
) -> (OnRequestMediaAccessFn, *mut c_void) {
    let handler = unsafe { get_permission_handler(callbacks.client_ptr()) };
    assert!(!handler.is_null(), "get_permission_handler returned null");
    let on_request: OnRequestMediaAccessFn = unsafe {
        std::mem::transmute(
            read_fn(
                handler,
                CEF_PERMISSION_HANDLER_ON_REQUEST_MEDIA_ACCESS_OFFSET,
            )
            .expect("on_request_media_access_permission slot"),
        )
    };
    (on_request, handler)
}

#[test]
fn before_unload_prompt_stashes_then_leave_continues_and_releases() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    *callbacks.state.nav_url.lock().expect("nav") = "https://editor.example/doc/1".to_owned();
    let (on_before_unload, handler) = resolve_on_before_unload(&callbacks);

    let message = CefStringOwned::new("You have unsaved changes").expect("message");
    let cef_callback = TestJsDialogCallback::new();
    let rv = unsafe {
        on_before_unload(
            handler,
            ptr::null_mut(),
            message.as_ptr().cast::<CefString>(),
            0,
            cef_callback.as_mut_ptr(),
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 0);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("beforeunload recv") else {
        panic!("expected beforeunload request")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::BeforeUnloadDialog {
            id: 1,
            message: "You have unsaved changes".to_owned(),
            origin: "https://editor.example/doc/1".to_owned(),
            is_reload: false,
        }
    );

    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::BeforeUnloadDecision {
            id: 1,
            proceed: true,
        },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.success.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);

    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::BeforeUnloadDecision {
            id: 1,
            proceed: false,
        },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn before_unload_prompt_teardown_answers_stay_and_releases() {
    use crate::sock::{recv, RecvOutcome};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let cef_callback = TestJsDialogCallback::new();
    {
        let callbacks = CefBrowserCallbacks::new(
            2,
            2,
            Some(&helper),
            noop_userfree_free,
            noop_string_list_size,
            noop_string_list_value,
        )
        .expect("callbacks");
        let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
            panic!("expected attach")
        };
        let (on_before_unload, handler) = resolve_on_before_unload(&callbacks);
        let message = CefStringOwned::new("Draft changed").expect("message");
        let rv = unsafe {
            on_before_unload(
                handler,
                ptr::null_mut(),
                message.as_ptr().cast::<CefString>(),
                1,
                cef_callback.as_mut_ptr(),
            )
        };
        assert_eq!(rv, 1);
        assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 1);
        assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 0);
    }

    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(
        cef_callback.success.load(Ordering::SeqCst),
        0,
        "teardown must cancel/stay, not leave the page"
    );
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn before_unload_prompts_are_bounded_and_overflow_stays_synchronously() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    *callbacks.state.nav_url.lock().expect("nav") = "https://editor.example/doc".to_owned();

    let mut held_callbacks = Vec::new();
    for idx in 0..MAX_PENDING_BEFORE_UNLOAD_DIALOGS {
        let callback = TestJsDialogCallback::new();
        let rv = callbacks.state.begin_before_unload_dialog(
            format!("draft {idx}"),
            false,
            callback.as_mut_ptr(),
        );
        assert_eq!(rv, 1);
        assert_eq!(callback.add_refs.load(Ordering::SeqCst), 1);
        assert_eq!(callback.conts.load(Ordering::SeqCst), 0);

        let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("beforeunload recv") else {
            panic!("expected beforeunload request")
        };
        assert!(fds.is_empty());
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        let expected_id = (idx + 1) as u64;
        assert_eq!(
            EventMsg::decode(&payload).expect("event"),
            EventMsg::BeforeUnloadDialog {
                id: expected_id,
                message: format!("draft {idx}"),
                origin: "https://editor.example/doc".to_owned(),
                is_reload: false,
            }
        );
        held_callbacks.push(callback);
    }
    assert_eq!(
        callbacks.state.pending_before_unload_count(),
        MAX_PENDING_BEFORE_UNLOAD_DIALOGS
    );

    let overflow = TestJsDialogCallback::new();
    let rv = callbacks.state.begin_before_unload_dialog(
        "overflow".to_owned(),
        false,
        overflow.as_mut_ptr(),
    );
    assert_eq!(rv, 1);
    assert_eq!(overflow.add_refs.load(Ordering::SeqCst), 0);
    assert_eq!(overflow.conts.load(Ordering::SeqCst), 1);
    assert_eq!(overflow.success.load(Ordering::SeqCst), 0);
    assert_eq!(overflow.releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        callbacks.state.pending_before_unload_count(),
        MAX_PENDING_BEFORE_UNLOAD_DIALOGS,
        "overflow must not grow the held CEF beforeunload callback set"
    );

    callbacks.apply_before_unload_decision(1, true);
    assert_eq!(held_callbacks[0].conts.load(Ordering::SeqCst), 1);
    assert_eq!(held_callbacks[0].success.load(Ordering::SeqCst), 1);
    assert_eq!(held_callbacks[0].releases.load(Ordering::SeqCst), 1);
    assert_eq!(
        callbacks.state.pending_before_unload_count(),
        MAX_PENDING_BEFORE_UNLOAD_DIALOGS - 1
    );

    let recovered = TestJsDialogCallback::new();
    let rv = callbacks.state.begin_before_unload_dialog(
        "recovered".to_owned(),
        false,
        recovered.as_mut_ptr(),
    );
    assert_eq!(rv, 1, "freeing a slot allows another beforeunload prompt");
    assert_eq!(recovered.add_refs.load(Ordering::SeqCst), 1);
    callbacks.state.release_pending_before_unload_dialogs();
    assert_eq!(callbacks.state.pending_before_unload_count(), 0);
}

#[test]
fn permission_handler_offsets_reconcile_with_the_pinned_cef_layout() {
    // get_permission_handler is index 10 of the CEF 149 client vtable — between
    // get_frame_handler (index 9, offset 112) and get_jsdialog_handler (index 11,
    // offset 128). Verified against /opt/mde/cef/include/capi/cef_client_capi.h.
    assert_eq!(CEF_CLIENT_GET_PERMISSION_HANDLER_OFFSET, 40 + 10 * 8);
    assert!(CEF_CLIENT_GET_PERMISSION_HANDLER_OFFSET > CEF_CLIENT_GET_FIND_HANDLER_OFFSET);
    assert!(CEF_CLIENT_GET_PERMISSION_HANDLER_OFFSET < CEF_CLIENT_GET_JSDIALOG_HANDLER_OFFSET);
    assert!(CEF_CLIENT_GET_PERMISSION_HANDLER_OFFSET < CEF_CLIENT_SIZE - 8);
    // cef_permission_handler_t: three methods after the 40-byte base.
    assert_eq!(CEF_PERMISSION_HANDLER_ON_REQUEST_MEDIA_ACCESS_OFFSET, 40);
    assert_eq!(CEF_PERMISSION_HANDLER_ON_SHOW_PROMPT_OFFSET, 40 + 8);
    assert_eq!(CEF_PERMISSION_HANDLER_ON_DISMISS_PROMPT_OFFSET, 40 + 2 * 8);
    assert_eq!(CEF_PERMISSION_HANDLER_SIZE, 40 + 3 * 8);
    assert!(CEF_PERMISSION_HANDLER_ON_DISMISS_PROMPT_OFFSET < CEF_PERMISSION_HANDLER_SIZE);
    // cef_permission_prompt_callback_t: cont right after the base.
    assert_eq!(CEF_PERMISSION_PROMPT_CALLBACK_CONT_OFFSET, 40);
    assert_eq!(CEF_PERMISSION_PROMPT_CALLBACK_SIZE, 40 + 8);
    // cef_media_access_callback_t: cont + cancel after the base.
    assert_eq!(CEF_MEDIA_ACCESS_CALLBACK_CONT_OFFSET, 40);
    assert_eq!(CEF_MEDIA_ACCESS_CALLBACK_CANCEL_OFFSET, 40 + 8);
    assert_eq!(CEF_MEDIA_ACCESS_CALLBACK_SIZE, 40 + 2 * 8);
    // cef_permission_request_result_t (ACCEPT=0, DENY=1, DISMISS=2, IGNORE=3).
    assert_eq!(CEF_PERMISSION_RESULT_ACCEPT, 0);
    assert_eq!(CEF_PERMISSION_RESULT_DENY, 1);
    // cef_permission_request_types_t bits (from cef_types.h).
    assert_eq!(CEF_PERMISSION_TYPE_CAMERA_STREAM, 1 << 2);
    assert_eq!(CEF_PERMISSION_TYPE_CLIPBOARD, 1 << 4);
    assert_eq!(CEF_PERMISSION_TYPE_GEOLOCATION, 1 << 8);
    assert_eq!(CEF_PERMISSION_TYPE_MIC_STREAM, 1 << 12);
    assert_eq!(CEF_PERMISSION_TYPE_MIDI_SYSEX, 1 << 13);
    assert_eq!(CEF_PERMISSION_TYPE_NOTIFICATIONS, 1 << 15);
    // cef_media_access_permission_types_t bits (from cef_types.h).
    assert_eq!(CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE, 1 << 0);
    assert_eq!(CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE, 1 << 1);
    assert_eq!(CEF_MEDIA_PERMISSION_DESKTOP_AUDIO_CAPTURE, 1 << 2);
    assert_eq!(CEF_MEDIA_PERMISSION_DESKTOP_VIDEO_CAPTURE, 1 << 3);
    // Permission bitmask → engine-neutral wire kind.
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_GEOLOCATION),
        Some(0)
    );
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_NOTIFICATIONS),
        Some(1)
    );
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_CLIPBOARD),
        Some(2)
    );
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_CAMERA_STREAM),
        Some(3)
    );
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_MIC_STREAM),
        Some(4)
    );
    assert_eq!(
        permission_kind_from_cef(
            CEF_PERMISSION_TYPE_CAMERA_STREAM | CEF_PERMISSION_TYPE_MIC_STREAM
        ),
        Some(5)
    );
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_MIDI_SYSEX),
        None
    );
    assert_eq!(permission_kind_from_cef(0), None); // NONE
                                                   // Several in-scope bits at once → earlier wire order wins.
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_GEOLOCATION | CEF_PERMISSION_TYPE_CLIPBOARD),
        Some(0)
    );
    assert_eq!(
        permission_kind_from_cef(CEF_PERMISSION_TYPE_NOTIFICATIONS | CEF_PERMISSION_TYPE_CLIPBOARD),
        Some(1)
    );
    assert_eq!(
        media_access_kind_from_cef(CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE),
        Some(3)
    );
    assert_eq!(
        media_access_kind_from_cef(CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE),
        Some(4)
    );
    assert_eq!(
        media_access_kind_from_cef(
            CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE | CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE
        ),
        Some(5)
    );
    assert_eq!(
        media_access_kind_from_cef(CEF_MEDIA_PERMISSION_DESKTOP_VIDEO_CAPTURE),
        None
    );
    assert_eq!(
        media_access_kind_from_cef(
            CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE | CEF_MEDIA_PERMISSION_DESKTOP_VIDEO_CAPTURE
        ),
        None
    );
}

#[test]
fn permission_prompt_geolocation_stashes_then_allow_accepts_and_releases() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let (on_show, handler) = resolve_on_show_permission_prompt(&callbacks);

    let origin = CefStringOwned::new("https://maps.example.com").expect("origin");
    let cef_callback = TestPermissionCallback::new();
    // A geolocation request is in scope → handled (return 1), callback add_ref'd +
    // stashed, PermissionRequest{kind:0} emitted.
    let rv = unsafe {
        on_show(
            handler,
            ptr::null_mut(),
            7,
            origin.as_ptr().cast::<CefString>(),
            CEF_PERMISSION_TYPE_GEOLOCATION,
            cef_callback.as_mut_ptr(),
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 0);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("permission recv") else {
        panic!("expected permission request")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PermissionRequest {
            id: 7,
            kind: 0,
            origin: "https://maps.example.com".to_owned(),
        }
    );

    // Shell allows → cont(ACCEPT) then release; the entry is cleared.
    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::PermissionDecision { id: 7, allow: true },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(
        cef_callback.result.load(Ordering::SeqCst),
        CEF_PERMISSION_RESULT_ACCEPT
    );
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);

    // A second decision for the same id is a safe no-op (already answered): no
    // second cont, no double release.
    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::PermissionDecision { id: 7, allow: true },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn permission_prompt_notifications_deny_denies_and_releases() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let (on_show, handler) = resolve_on_show_permission_prompt(&callbacks);

    let origin = CefStringOwned::new("https://news.example.org").expect("origin");
    let cef_callback = TestPermissionCallback::new();
    let rv = unsafe {
        on_show(
            handler,
            ptr::null_mut(),
            11,
            origin.as_ptr().cast::<CefString>(),
            CEF_PERMISSION_TYPE_NOTIFICATIONS,
            cef_callback.as_mut_ptr(),
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 1);

    let RecvOutcome::Data { bytes, .. } = recv(&shell).expect("permission recv") else {
        panic!("expected permission request")
    };
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    assert_eq!(
        EventMsg::decode(&payload).expect("event"),
        EventMsg::PermissionRequest {
            id: 11,
            kind: 1,
            origin: "https://news.example.org".to_owned(),
        }
    );

    // Shell denies → cont(DENY) then release.
    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::PermissionDecision {
            id: 11,
            allow: false,
        },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(
        cef_callback.result.load(Ordering::SeqCst),
        CEF_PERMISSION_RESULT_DENY
    );
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn media_access_permission_camera_mic_prompts_then_allows_requested_bits() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let (on_request, handler) = resolve_on_request_media_access(&callbacks);

    let origin = CefStringOwned::new("https://meet.example.com").expect("origin");
    let cef_callback = TestMediaAccessCallback::new();
    let requested_permissions =
        CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE | CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE;
    let rv = unsafe {
        on_request(
            handler,
            ptr::null_mut(),
            ptr::null_mut(),
            origin.as_ptr().cast::<CefString>(),
            requested_permissions,
            cef_callback.as_mut_ptr(),
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 0);

    let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("media permission recv") else {
        panic!("expected media permission request")
    };
    assert!(fds.is_empty());
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    let EventMsg::PermissionRequest { id, kind, origin } =
        EventMsg::decode(&payload).expect("event")
    else {
        panic!("expected permission request");
    };
    assert_eq!(id, MEDIA_PERMISSION_ID_BASE);
    assert_eq!(kind, 5);
    assert_eq!(origin, "https://meet.example.com");

    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::PermissionDecision { id, allow: true },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(
        cef_callback.allowed.load(Ordering::SeqCst),
        requested_permissions,
        "CEF media access grants must echo exactly the requested device bits"
    );
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);

    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::PermissionDecision { id, allow: true },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn media_access_permission_microphone_deny_returns_zero_allowed_bits() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let (on_request, handler) = resolve_on_request_media_access(&callbacks);

    let origin = CefStringOwned::new("https://voice.example.com").expect("origin");
    let cef_callback = TestMediaAccessCallback::new();
    let rv = unsafe {
        on_request(
            handler,
            ptr::null_mut(),
            ptr::null_mut(),
            origin.as_ptr().cast::<CefString>(),
            CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE,
            cef_callback.as_mut_ptr(),
        )
    };
    assert_eq!(rv, 1);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 1);

    let RecvOutcome::Data { bytes, .. } = recv(&shell).expect("media permission recv") else {
        panic!("expected media permission request")
    };
    let mut bytes = bytes;
    let payload = take_frame(&mut bytes).expect("frame").expect("payload");
    let EventMsg::PermissionRequest { id, kind, origin } =
        EventMsg::decode(&payload).expect("event")
    else {
        panic!("expected permission request");
    };
    assert_eq!(id, MEDIA_PERMISSION_ID_BASE);
    assert_eq!(kind, 4);
    assert_eq!(origin, "https://voice.example.com");

    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::PermissionDecision { id, allow: false },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 1);
    assert_eq!(cef_callback.allowed.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn permission_prompts_are_bounded_and_overflow_denies_synchronously() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let mut held_callbacks = Vec::new();
    for idx in 0..MAX_PENDING_PERMISSION_PROMPTS {
        let callback = TestPermissionCallback::new();
        let id = idx as u64;
        let rv = callbacks.state.begin_permission_prompt(
            id,
            "https://maps.example".to_owned(),
            CEF_PERMISSION_TYPE_GEOLOCATION,
            callback.as_mut_ptr(),
        );
        assert_eq!(rv, 1);
        assert_eq!(callback.add_refs.load(Ordering::SeqCst), 1);
        assert_eq!(callback.conts.load(Ordering::SeqCst), 0);

        let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("permission recv") else {
            panic!("expected permission request")
        };
        assert!(fds.is_empty());
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        assert_eq!(
            EventMsg::decode(&payload).expect("event"),
            EventMsg::PermissionRequest {
                id,
                kind: 0,
                origin: "https://maps.example".to_owned(),
            }
        );
        held_callbacks.push(callback);
    }
    assert_eq!(
        callbacks.state.pending_permission_prompt_count(),
        MAX_PENDING_PERMISSION_PROMPTS
    );

    let overflow = TestPermissionCallback::new();
    let rv = callbacks.state.begin_permission_prompt(
        999,
        "https://maps.example".to_owned(),
        CEF_PERMISSION_TYPE_GEOLOCATION,
        overflow.as_mut_ptr(),
    );
    assert_eq!(rv, 1);
    assert_eq!(overflow.add_refs.load(Ordering::SeqCst), 0);
    assert_eq!(overflow.conts.load(Ordering::SeqCst), 1);
    assert_eq!(
        overflow.result.load(Ordering::SeqCst),
        CEF_PERMISSION_RESULT_DENY
    );
    assert_eq!(overflow.releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        callbacks.state.pending_permission_prompt_count(),
        MAX_PENDING_PERMISSION_PROMPTS,
        "overflow must not grow the held CEF permission callback set"
    );

    callbacks.apply_permission_decision(0, true);
    assert_eq!(held_callbacks[0].conts.load(Ordering::SeqCst), 1);
    assert_eq!(
        held_callbacks[0].result.load(Ordering::SeqCst),
        CEF_PERMISSION_RESULT_ACCEPT
    );
    assert_eq!(held_callbacks[0].releases.load(Ordering::SeqCst), 1);
    assert_eq!(
        callbacks.state.pending_permission_prompt_count(),
        MAX_PENDING_PERMISSION_PROMPTS - 1
    );

    let recovered = TestPermissionCallback::new();
    let rv = callbacks.state.begin_permission_prompt(
        1000,
        "https://maps.example".to_owned(),
        CEF_PERMISSION_TYPE_GEOLOCATION,
        recovered.as_mut_ptr(),
    );
    assert_eq!(rv, 1, "freeing a slot allows another permission prompt");
    assert_eq!(recovered.add_refs.load(Ordering::SeqCst), 1);
    callbacks.state.release_pending_permission_prompts();
    assert_eq!(callbacks.state.pending_permission_prompt_count(), 0);
}

#[test]
fn permission_prompt_out_of_scope_types_default_deny_without_emitting() {
    use crate::sock::{recv, RecvOutcome};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks = CefBrowserCallbacks::new(
        2,
        2,
        Some(&helper),
        noop_userfree_free,
        noop_string_list_size,
        noop_string_list_value,
    )
    .expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    shell.set_nonblocking(true).expect("nonblocking");
    let (on_show, handler) = resolve_on_show_permission_prompt(&callbacks);

    let origin = CefStringOwned::new("https://midi.example.com").expect("origin");
    let cef_callback = TestPermissionCallback::new();
    // MIDI sysex remains out of scope → return 0 (default handling / deny under
    // Alloy style): no add_ref, no stash, nothing published.
    let rv = unsafe {
        on_show(
            handler,
            ptr::null_mut(),
            3,
            origin.as_ptr().cast::<CefString>(),
            CEF_PERMISSION_TYPE_MIDI_SYSEX,
            cef_callback.as_mut_ptr(),
        )
    };
    assert_eq!(rv, 0);
    assert_eq!(cef_callback.add_refs.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 0);
    // Nothing was emitted to the shell.
    assert!(matches!(recv(&shell), Ok(RecvOutcome::WouldBlock)));

    // A decision for an id we never stashed is a safe no-op.
    apply_control_frame(
        ptr::null_mut(),
        &callbacks,
        &ControlMsg::PermissionDecision { id: 3, allow: true },
    );
    assert_eq!(cef_callback.conts.load(Ordering::SeqCst), 0);
    assert_eq!(cef_callback.releases.load(Ordering::SeqCst), 0);
}

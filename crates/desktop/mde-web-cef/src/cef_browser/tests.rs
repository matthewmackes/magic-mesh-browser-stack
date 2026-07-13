//! Unit tests for the CEF browser bridge (relocated verbatim from mod.rs).
use super::*;
use std::mem::{align_of, size_of};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    assert_eq!(CEF_BROWSER_HOST_PRINT_OFFSET, 504);
    assert_eq!(CEF_BROWSER_HOST_PRINT_TO_PDF_OFFSET, 512);
    assert_eq!(CEF_BROWSER_HOST_SET_AUDIO_MUTED_OFFSET, 520);
    assert_eq!(CEF_BROWSER_HOST_IS_AUDIO_MUTED_OFFSET, 528);
    assert_eq!(CEF_FRAME_SIZE, 248);
    assert_eq!(CEF_FRAME_LOAD_URL_OFFSET, 144);
    assert_eq!(CEF_FRAME_EXECUTE_JAVA_SCRIPT_OFFSET, 152);
    assert_eq!(CEF_REQUEST_SIZE, 216);
    assert_eq!(CEF_REQUEST_GET_URL_OFFSET, 48);
    assert_eq!(CEF_CALLBACK_SIZE, 56);
    assert_eq!(CEF_CALLBACK_CONT_OFFSET, 40);
    assert_eq!(CEF_CALLBACK_CANCEL_OFFSET, 48);
    assert_eq!(CEF_PDF_PRINT_CALLBACK_SIZE, 48);
    assert_eq!(CEF_PDF_PRINT_CALLBACK_ON_FINISHED_OFFSET, 40);
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
        KEYEVENT_CHAR,
        EVENTFLAG_SHIFT_DOWN,
        65,
        65,
        b'A' as u16,
        b'A' as u16,
    );
    assert_eq!(
        read_i32(&event.bytes, CEF_KEY_EVENT_TYPE_OFFSET),
        KEYEVENT_CHAR
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
static STOP_LOAD_CALLS: AtomicI32 = AtomicI32::new(0);
static AUDIO_MUTED_CALLS: AtomicI32 = AtomicI32::new(0);
static AUDIO_MUTED_LAST: AtomicI32 = AtomicI32::new(0);
static TEST_HOST_PTR: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn test_browser_host(_browser: *mut c_void) -> *mut c_void {
    TEST_HOST_PTR.load(AtomicOrdering::SeqCst) as *mut c_void
}

unsafe extern "C" fn record_focus(_host: *mut c_void, focused: c_int) {
    FOCUS_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    FOCUS_LAST.store(focused, AtomicOrdering::SeqCst);
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
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");

    apply_control_frame(browser.as_mut_ptr().cast(), &callbacks, &ControlMsg::Stop);

    assert_eq!(STOP_LOAD_CALLS.load(AtomicOrdering::SeqCst), 1);
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
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");

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
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");

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
fn page_zoom_and_find_scripts_are_bounded_and_escaped() {
    assert!(page_zoom_script(125).contains("zoom='125%'"));
    assert!(page_zoom_script(5).contains("zoom='25%'"));
    assert!(page_zoom_script(900).contains("zoom='500%'"));

    let forward = find_in_page_script("mesh \"ops\"", false);
    assert!(forward.contains(r#"window.find("mesh \"ops\"",false,false"#));
    let backward = find_in_page_script("mesh", true);
    assert!(backward.contains(r#"window.find("mesh",false,true"#));
    assert!(clear_find_script().contains("removeAllRanges"));
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
    // its doc comment); this renderer-level shim is the real mitigation,
    // so pin exactly which JS-reachable entry points it removes.
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
fn pump_interval_backs_off_when_idle_but_stays_fast_while_active() {
    // perf-6: awaiting the first paint is always active regardless of the
    // idle clock, so initial load latency is never regressed.
    assert_eq!(pump_interval(Duration::from_secs(30), true), PUMP_ACTIVE);
    // Recent activity (paint/frame/nav within the grace window) stays fast.
    assert_eq!(pump_interval(Duration::ZERO, false), PUMP_ACTIVE);
    assert_eq!(pump_interval(PUMP_IDLE_AFTER / 2, false), PUMP_ACTIVE);
    // Sustained quiet backs off so an idle tab stops spinning at 125 Hz.
    assert_eq!(pump_interval(PUMP_IDLE_AFTER, false), PUMP_IDLE);
    assert_eq!(pump_interval(Duration::from_secs(5), false), PUMP_IDLE);
    // The idle interval is a real, substantial back-off from the active spin.
    assert!(PUMP_IDLE >= PUMP_ACTIVE * 10);
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
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");
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
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");
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
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");
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
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");
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
    let callbacks =
        CefBrowserCallbacks::new(2, 2, Some(&helper), noop_userfree_free).expect("callbacks");

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
    let callbacks =
        CefBrowserCallbacks::new(2, 2, Some(&helper), noop_userfree_free).expect("callbacks");

    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let callback = callbacks.retain_pdf_callback();
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
    let _ = std::fs::remove_file(path);
}

#[test]
fn pdf_completion_callback_rejects_missing_pdf_output() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks =
        CefBrowserCallbacks::new(2, 2, Some(&helper), noop_userfree_free).expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };
    let callback = callbacks.retain_pdf_callback();
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
}

#[test]
fn request_handler_registry_returns_resource_request_peer() {
    let callbacks =
        CefBrowserCallbacks::new(320, 200, None, noop_userfree_free).expect("callbacks");
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
    let callbacks =
        CefBrowserCallbacks::new(2, 2, Some(&helper), noop_userfree_free).expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "https://www.google-analytics.com/collect".to_owned(),
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
fn page_text_beacon_is_intercepted_and_published_without_adfilter_roundtrip() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks =
        CefBrowserCallbacks::new(2, 2, Some(&helper), noop_userfree_free).expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "mde-page-text://capture/77?text=hello%20page".to_owned(),
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
fn page_scrape_beacon_is_intercepted_and_published_without_adfilter_roundtrip() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks =
        CefBrowserCallbacks::new(2, 2, Some(&helper), noop_userfree_free).expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
            "https://mde-page-scrape.invalid/capture/88?body=%7B%22text%22%3A%22hello%22%2C%22links%22%3A%5B%5D%7D".to_owned(),
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
fn passkey_beacon_is_intercepted_and_published_without_adfilter_roundtrip() {
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    let (helper, shell) = UnixStream::pair().expect("socketpair");
    let callbacks =
        CefBrowserCallbacks::new(2, 2, Some(&helper), noop_userfree_free).expect("callbacks");
    let RecvOutcome::Data { .. } = recv(&shell).expect("attach recv") else {
        panic!("expected attach")
    };

    let cef_callback = TestCefCallback::new();
    let rv = callbacks.state.begin_resource_request(
        "https://mde-passkey.invalid/request/?body=%7B%22ceremony%22%3A%22get%22%7D".to_owned(),
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

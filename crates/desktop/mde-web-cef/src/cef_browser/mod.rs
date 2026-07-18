//! Header-pinned CEF windowless browser creation for the native bridge.
//!
//! This module carries only the C callback surface needed to prove that the
//! pinned CEF 149 runtime can create an offscreen browser and invoke paint. The
//! full shell socket lifecycle is intentionally a later slice; this keeps the
//! probe honest while replacing the previous "offscreen pending" blocker with a
//! real browser-process boundary.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::raw::c_int;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::cef_abi::{CefAbi, CefStringListSize, CefStringListValue, CefStringUserfreeUtf16Free};
use crate::offscreen::{OffscreenError, OffscreenFrameSink};
use crate::sock::{self, RecvOutcome};
use crate::wire::{
    self, ControlMsg, CursorKind, EditCommand, EventMsg, InputEvent, KeyCode, MediaTransportAction,
    Modifiers, PointerButton,
};

mod scripts;
use scripts::*;

/// `sizeof(cef_base_ref_counted_t)` for pinned Linux CEF 149.
pub const CEF_BASE_REF_COUNTED_SIZE: usize = 40;
/// `sizeof(cef_window_info_t)` for pinned Linux CEF 149.
pub const CEF_WINDOW_INFO_SIZE: usize = 88;
/// `offsetof(cef_window_info_t, bounds)`.
pub const CEF_WINDOW_INFO_BOUNDS_OFFSET: usize = 32;
/// `offsetof(cef_window_info_t, windowless_rendering_enabled)`.
pub const CEF_WINDOW_INFO_WINDOWLESS_OFFSET: usize = 56;
/// `offsetof(cef_window_info_t, shared_texture_enabled)`.
pub const CEF_WINDOW_INFO_SHARED_TEXTURE_OFFSET: usize = 60;
/// `offsetof(cef_window_info_t, external_begin_frame_enabled)`.
pub const CEF_WINDOW_INFO_EXTERNAL_BEGIN_FRAME_OFFSET: usize = 64;
/// `offsetof(cef_window_info_t, runtime_style)`.
pub const CEF_WINDOW_INFO_RUNTIME_STYLE_OFFSET: usize = 80;
/// `cef_runtime_style_t::CEF_RUNTIME_STYLE_ALLOY` for pinned Linux CEF 149.
pub const CEF_RUNTIME_STYLE_ALLOY: i32 = 2;
/// `sizeof(cef_browser_settings_t)` for pinned Linux CEF 149.
pub const CEF_BROWSER_SETTINGS_SIZE: usize = 264;
/// `offsetof(cef_browser_settings_t, windowless_frame_rate)`.
pub const CEF_BROWSER_SETTINGS_FRAME_RATE_OFFSET: usize = 8;
/// `offsetof(cef_browser_settings_t, background_color)`.
pub const CEF_BROWSER_SETTINGS_BACKGROUND_COLOR_OFFSET: usize = 248;
/// `sizeof(cef_client_t)` for pinned Linux CEF 149.
pub const CEF_CLIENT_SIZE: usize = 192;
/// `offsetof(cef_client_t, get_life_span_handler)`.
pub const CEF_CLIENT_GET_LIFE_SPAN_HANDLER_OFFSET: usize = 144;
/// `offsetof(cef_client_t, get_print_handler)`.
pub const CEF_CLIENT_GET_PRINT_HANDLER_OFFSET: usize = 160;
/// `offsetof(cef_client_t, get_render_handler)`.
pub const CEF_CLIENT_GET_RENDER_HANDLER_OFFSET: usize = 168;
/// `offsetof(cef_client_t, get_request_handler)`.
pub const CEF_CLIENT_GET_REQUEST_HANDLER_OFFSET: usize = 176;
/// `offsetof(cef_client_t, get_audio_handler)` — the FIRST handler getter, right
/// after the 40-byte `cef_base_ref_counted_t` base (index 0 → 40 + 0*8 = 40).
/// The rest of the frozen CEF 149 client vtable pins this: get_display_handler=72
/// (index 4), get_download_handler=80 (5), get_find_handler=96 (7),
/// get_jsdialog_handler=128 (11), get_life_span_handler=144 (13), get_load_handler
/// =152 (14), get_request_handler=176 (17) all land exactly on `40 + index*8`, and
/// `CEF_CLIENT_SIZE`=192 = 40 + 19*8 caps the 19-method struct — so audio sits at 40.
pub const CEF_CLIENT_GET_AUDIO_HANDLER_OFFSET: usize = 40;
/// `offsetof(cef_client_t, get_display_handler)` — carries nav/title state (B1).
pub const CEF_CLIENT_GET_DISPLAY_HANDLER_OFFSET: usize = 72;
/// `offsetof(cef_client_t, get_find_handler)` — find-in-page match results (field 7).
pub const CEF_CLIENT_GET_FIND_HANDLER_OFFSET: usize = 96;
/// `offsetof(cef_client_t, get_download_handler)` — download interception (field 5).
pub const CEF_CLIENT_GET_DOWNLOAD_HANDLER_OFFSET: usize = 80;
/// `offsetof(cef_client_t, get_jsdialog_handler)` — alert()/confirm()/prompt()
/// handler (index 11 in the pinned CEF 149 client vtable: base 40 + 11*8 = 128,
/// between get_permission_handler=120 and get_keyboard_handler=136).
pub const CEF_CLIENT_GET_JSDIALOG_HANDLER_OFFSET: usize = 128;
/// `sizeof(cef_jsdialog_handler_t)` (4 fn ptrs + 40 base): on_jsdialog(40),
/// on_before_unload_dialog(48), on_reset_dialog_state(56), on_dialog_closed(64).
/// We register on_jsdialog + on_before_unload_dialog; reset/closed stay null.
pub const CEF_JSDIALOG_HANDLER_SIZE: usize = 72;
/// `offsetof(cef_jsdialog_handler_t, on_jsdialog)`.
pub const CEF_JSDIALOG_HANDLER_ON_JSDIALOG_OFFSET: usize = 40;
/// `offsetof(cef_jsdialog_handler_t, on_before_unload_dialog)`.
pub const CEF_JSDIALOG_HANDLER_ON_BEFORE_UNLOAD_DIALOG_OFFSET: usize = 48;
/// `sizeof(cef_download_handler_t)` (3 fn ptrs + 40 base).
pub const CEF_DOWNLOAD_HANDLER_SIZE: usize = 64;
/// `offsetof(cef_download_handler_t, can_download)`.
pub const CEF_DOWNLOAD_HANDLER_CAN_DOWNLOAD_OFFSET: usize = 40;
/// `offsetof(cef_download_handler_t, on_before_download)`.
pub const CEF_DOWNLOAD_HANDLER_ON_BEFORE_DOWNLOAD_OFFSET: usize = 48;
/// `offsetof(cef_download_item_t, get_url)`.
pub const CEF_DOWNLOAD_ITEM_GET_URL_OFFSET: usize = 152;
/// `offsetof(cef_download_item_t, get_suggested_file_name)`.
pub const CEF_DOWNLOAD_ITEM_GET_SUGGESTED_FILE_NAME_OFFSET: usize = 168;
/// `sizeof(cef_find_handler_t)` (1 fn ptr + 40 base).
pub const CEF_FIND_HANDLER_SIZE: usize = 48;
/// `offsetof(cef_find_handler_t, on_find_result)`.
pub const CEF_FIND_HANDLER_ON_FIND_RESULT_OFFSET: usize = 40;
/// `sizeof(cef_string_visitor_t)` (1 fn ptr + 40 base).
pub const CEF_STRING_VISITOR_SIZE: usize = 48;
/// `offsetof(cef_string_visitor_t, visit)`.
pub const CEF_STRING_VISITOR_VISIT_OFFSET: usize = 40;
/// `offsetof(cef_client_t, get_load_handler)` — carries loading/back/forward (B1).
pub const CEF_CLIENT_GET_LOAD_HANDLER_OFFSET: usize = 152;
/// `sizeof(cef_display_handler_t)` for pinned Linux CEF 149 (13 fn ptrs + 40-byte base).
pub const CEF_DISPLAY_HANDLER_SIZE: usize = 144;
/// `offsetof(cef_display_handler_t, on_address_change)`.
pub const CEF_DISPLAY_HANDLER_ON_ADDRESS_CHANGE_OFFSET: usize = 40;
/// `offsetof(cef_display_handler_t, on_title_change)`.
pub const CEF_DISPLAY_HANDLER_ON_TITLE_CHANGE_OFFSET: usize = 48;
/// `offsetof(cef_display_handler_t, on_cursor_change)` — engine cursor shape
/// (field 9; on_address_change=40 pins field 0, on_title_change=48 field 1).
pub const CEF_DISPLAY_HANDLER_ON_CURSOR_CHANGE_OFFSET: usize = 112;
/// `offsetof(cef_display_handler_t, on_favicon_urlchange)` — the page's favicon
/// URLs changed (field 2, right after on_title_change=48). Signature
/// `void(self, cef_browser_t*, cef_string_list_t icon_urls)`.
pub const CEF_DISPLAY_HANDLER_ON_FAVICON_URLCHANGE_OFFSET: usize = 56;
/// `offsetof(cef_display_handler_t, on_fullscreen_mode_change)` — field 3, right
/// after on_favicon_urlchange=56 (base 40 + 3*8). Signature
/// `void(self, cef_browser_t*, int fullscreen)`.
pub const CEF_DISPLAY_HANDLER_ON_FULLSCREEN_MODE_CHANGE_OFFSET: usize = 64;
/// `sizeof(cef_load_handler_t)` for pinned Linux CEF 149 (4 fn ptrs + 40-byte base).
pub const CEF_LOAD_HANDLER_SIZE: usize = 72;
/// `offsetof(cef_load_handler_t, on_loading_state_change)`.
pub const CEF_LOAD_HANDLER_ON_LOADING_STATE_CHANGE_OFFSET: usize = 40;
/// `sizeof(cef_audio_handler_t)` for pinned Linux CEF 149 (5 fn ptrs + 40-byte
/// base = 40 + 5*8): get_audio_parameters, on_audio_stream_{started,packet,
/// stopped,error}. Resolved via a dedicated cached pointer (`audio_handler_ptr`),
/// never the size-keyed `lookup_peer`, so this 80 need not join its whitelist.
pub const CEF_AUDIO_HANDLER_SIZE: usize = 80;
/// `offsetof(cef_audio_handler_t, get_audio_parameters)` — index 0 (40 + 0*8).
pub const CEF_AUDIO_HANDLER_GET_AUDIO_PARAMETERS_OFFSET: usize = 40;
/// `offsetof(cef_audio_handler_t, on_audio_stream_started)` — index 1 (40 + 1*8).
pub const CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STARTED_OFFSET: usize = 48;
/// `offsetof(cef_audio_handler_t, on_audio_stream_packet)` — index 2 (40 + 2*8).
pub const CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_PACKET_OFFSET: usize = 56;
/// `offsetof(cef_audio_handler_t, on_audio_stream_stopped)` — index 3 (40 + 3*8).
pub const CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STOPPED_OFFSET: usize = 64;
/// `offsetof(cef_audio_handler_t, on_audio_stream_error)` — index 4 (40 + 4*8).
pub const CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_ERROR_OFFSET: usize = 72;
/// `sizeof(cef_audio_parameters_t)` for pinned Linux CEF 149. The struct leads
/// with a `size_t size` (like every sized CEF POD), then three ints:
/// `channel_layout` (a `cef_channel_layout_t` enum = int)@8, `sample_rate`@12,
/// `frames_per_buffer`@16 — `8 + 3*4 = 20`, padded to `size_t` alignment → 24.
/// Corrected from a stale 12 that omitted the leading `size`, which shifted every
/// field write in `get_audio_parameters` by 8 bytes on live CEF (channel_layout
/// landed in `size`, sample_rate/frames_per_buffer never reached their slots).
/// Verified against pinned `internal/cef_types.h` `_cef_audio_parameters_t`.
pub const CEF_AUDIO_PARAMETERS_SIZE: usize = 24;
/// `CEF_CHANNEL_LAYOUT_STEREO` from the CEF 149 `cef_channel_layout_t` enum
/// (NONE=0, UNSUPPORTED=1, MONO=2, STEREO=3) — the sane default we request so CEF
/// actually spins up an audio stream and fires the started/stopped callbacks.
pub const CEF_CHANNEL_LAYOUT_STEREO: i32 = 3;
/// `offsetof(cef_client_t, get_permission_handler)` — per-site geolocation /
/// notifications / clipboard grants (index 10 in the pinned CEF 149 client vtable:
/// base 40 + 10*8 = 120, between get_frame_handler=112 (index 9) and
/// get_jsdialog_handler=128 (index 11)). Verified against the frozen
/// `cef_client_capi.h` getter order (audio0 command1 context_menu2 dialog3
/// display4 download5 drag6 find7 focus8 frame9 permission10 jsdialog11 …), which
/// pins every in-repo client anchor exactly on `40 + index*8`. Carried on a
/// dedicated cached pointer (`permission_handler_ptr`), never `lookup_peer`.
pub const CEF_CLIENT_GET_PERMISSION_HANDLER_OFFSET: usize = 120;
/// `sizeof(cef_permission_handler_t)` for pinned Linux CEF 149 (3 fn ptrs + 40
/// base = 40 + 3*8): on_request_media_access_permission(40),
/// on_show_permission_prompt(48), on_dismiss_permission_prompt(56).
pub const CEF_PERMISSION_HANDLER_SIZE: usize = 64;
/// `offsetof(cef_permission_handler_t, on_request_media_access_permission)` —
/// index 0. Camera/mic (getUserMedia) uses a separate CEF media-access callback
/// whose `Continue` argument is an allowed-permissions bitmask, not the generic
/// `cef_permission_request_result_t`.
pub const CEF_PERMISSION_HANDLER_ON_REQUEST_MEDIA_ACCESS_OFFSET: usize = 40;
/// `offsetof(cef_permission_handler_t, on_show_permission_prompt)` — index 1.
/// Signature `int(self, browser, prompt_id: uint64, requesting_origin:
/// cef_string_t*, requested_permissions: uint32, callback:
/// cef_permission_prompt_callback_t*)`.
pub const CEF_PERMISSION_HANDLER_ON_SHOW_PROMPT_OFFSET: usize = 48;
/// `offsetof(cef_permission_handler_t, on_dismiss_permission_prompt)` — index 2.
/// Signature `void(self, browser, prompt_id: uint64, result:
/// cef_permission_request_result_t)`.
pub const CEF_PERMISSION_HANDLER_ON_DISMISS_PROMPT_OFFSET: usize = 56;
/// `sizeof(cef_permission_prompt_callback_t)` for pinned Linux CEF 149 (1 fn ptr +
/// 40 base = 48): only `cont`.
pub const CEF_PERMISSION_PROMPT_CALLBACK_SIZE: usize = 48;
/// `offsetof(cef_permission_prompt_callback_t, cont)` — index 0, right after the
/// 40-byte base. Signature `void(self, result: cef_permission_request_result_t)`.
pub const CEF_PERMISSION_PROMPT_CALLBACK_CONT_OFFSET: usize = 40;
/// `sizeof(cef_media_access_callback_t)` for pinned Linux CEF 149 (2 fn ptrs +
/// 40 base = 56): `cont`, then `cancel`.
pub const CEF_MEDIA_ACCESS_CALLBACK_SIZE: usize = 56;
/// `offsetof(cef_media_access_callback_t, cont)` — index 0, right after the
/// 40-byte base. Signature `void(self, allowed_permissions: uint32_t)`.
pub const CEF_MEDIA_ACCESS_CALLBACK_CONT_OFFSET: usize = 40;
/// `offsetof(cef_media_access_callback_t, cancel)` — index 1.
pub const CEF_MEDIA_ACCESS_CALLBACK_CANCEL_OFFSET: usize = 48;
/// `cef_permission_request_result_t::CEF_PERMISSION_RESULT_ACCEPT` (enum value 0)
/// — grant the permission as if the user allowed it.
pub const CEF_PERMISSION_RESULT_ACCEPT: c_int = 0;
/// `cef_permission_request_result_t::CEF_PERMISSION_RESULT_DENY` (enum value 1) —
/// deny the permission as if the user denied it (ACCEPT=0, DENY=1, DISMISS=2,
/// IGNORE=3).
pub const CEF_PERMISSION_RESULT_DENY: c_int = 1;
/// `cef_permission_request_types_t::CEF_PERMISSION_TYPE_CLIPBOARD` (1 << 4) — the
/// async Clipboard API read/write bit in `requested_permissions`.
pub const CEF_PERMISSION_TYPE_CLIPBOARD: u32 = 1 << 4;
/// `cef_permission_request_types_t::CEF_PERMISSION_TYPE_CAMERA_STREAM` (1 << 2).
pub const CEF_PERMISSION_TYPE_CAMERA_STREAM: u32 = 1 << 2;
/// `cef_permission_request_types_t::CEF_PERMISSION_TYPE_GEOLOCATION` (1 << 8).
pub const CEF_PERMISSION_TYPE_GEOLOCATION: u32 = 1 << 8;
/// `cef_permission_request_types_t::CEF_PERMISSION_TYPE_MIC_STREAM` (1 << 12).
pub const CEF_PERMISSION_TYPE_MIC_STREAM: u32 = 1 << 12;
/// `cef_permission_request_types_t::CEF_PERMISSION_TYPE_NOTIFICATIONS` (1 << 15).
pub const CEF_PERMISSION_TYPE_NOTIFICATIONS: u32 = 1 << 15;
/// `cef_permission_request_types_t::CEF_PERMISSION_TYPE_MIDI_SYSEX` (1 << 13).
pub const CEF_PERMISSION_TYPE_MIDI_SYSEX: u32 = 1 << 13;
/// `cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE`.
pub const CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE: u32 = 1 << 0;
/// `cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE`.
pub const CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE: u32 = 1 << 1;
/// `cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DESKTOP_AUDIO_CAPTURE`.
pub const CEF_MEDIA_PERMISSION_DESKTOP_AUDIO_CAPTURE: u32 = 1 << 2;
/// `cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DESKTOP_VIDEO_CAPTURE`.
pub const CEF_MEDIA_PERMISSION_DESKTOP_VIDEO_CAPTURE: u32 = 1 << 3;
/// Engine-neutral permission `kind` on the wire (mirrors
/// [`wire::EventMsg::PermissionRequest`]): geolocation.
const PERMISSION_KIND_GEOLOCATION: u8 = 0;
/// Engine-neutral permission `kind` on the wire: notifications.
const PERMISSION_KIND_NOTIFICATIONS: u8 = 1;
/// Engine-neutral permission `kind` on the wire: clipboard.
const PERMISSION_KIND_CLIPBOARD: u8 = 2;
/// Engine-neutral permission `kind` on the wire: camera stream.
const PERMISSION_KIND_CAMERA: u8 = 3;
/// Engine-neutral permission `kind` on the wire: microphone stream.
const PERMISSION_KIND_MICROPHONE: u8 = 4;
/// Engine-neutral permission `kind` on the wire: camera + microphone stream.
const PERMISSION_KIND_CAMERA_MICROPHONE: u8 = 5;
/// Bridge-minted ids for CEF media-access prompts live above CEF's normal
/// `on_show_permission_prompt` ids so both callback classes share one pending map.
const MEDIA_PERMISSION_ID_BASE: u64 = 1u64 << 63;
/// `sizeof(cef_life_span_handler_t)` for pinned Linux CEF 149.
pub const CEF_LIFE_SPAN_HANDLER_SIZE: usize = 88;
/// `offsetof(cef_life_span_handler_t, on_after_created)`.
pub const CEF_LIFE_SPAN_ON_AFTER_CREATED_OFFSET: usize = 64;
/// `offsetof(cef_life_span_handler_t, on_before_close)`.
pub const CEF_LIFE_SPAN_ON_BEFORE_CLOSE_OFFSET: usize = 80;
/// `offsetof(cef_life_span_handler_t, on_before_popup)` — window.open /
/// target=_blank interception (field 0; on_after_created=64 pins field 3).
pub const CEF_LIFE_SPAN_ON_BEFORE_POPUP_OFFSET: usize = 40;
/// `sizeof(cef_render_handler_t)` for pinned Linux CEF 149.
pub const CEF_RENDER_HANDLER_SIZE: usize = 176;
/// `offsetof(cef_render_handler_t, get_view_rect)`.
pub const CEF_RENDER_HANDLER_GET_VIEW_RECT_OFFSET: usize = 56;
/// `offsetof(cef_render_handler_t, on_paint)`.
pub const CEF_RENDER_HANDLER_ON_PAINT_OFFSET: usize = 96;
/// `offsetof(cef_render_handler_t, on_popup_show)` — the engine's popup widget
/// (`<select>` dropdown / autocomplete list) toggled visible.
pub const CEF_RENDER_HANDLER_ON_POPUP_SHOW_OFFSET: usize = 80;
/// `offsetof(cef_render_handler_t, on_popup_size)` — the popup widget's view rect.
pub const CEF_RENDER_HANDLER_ON_POPUP_SIZE_OFFSET: usize = 88;
/// `sizeof(cef_request_handler_t)` for pinned Linux CEF 149.
pub const CEF_REQUEST_HANDLER_SIZE: usize = 128;
/// `offsetof(cef_request_handler_t, get_resource_request_handler)`.
pub const CEF_REQUEST_HANDLER_GET_RESOURCE_REQUEST_HANDLER_OFFSET: usize = 56;
/// `offsetof(cef_request_handler_t, on_certificate_error)` for pinned Linux CEF
/// 149 — a TLS/certificate validation failure on the top-level load. The proven
/// get_resource_request_handler=56 pins index 2, so on_certificate_error (index
/// 4: get_auth_credentials@64, on_certificate_error@72) sits here.
pub const CEF_REQUEST_HANDLER_ON_CERTIFICATE_ERROR_OFFSET: usize = 72;
/// `offsetof(cef_request_handler_t, on_render_process_terminated)` — the
/// renderer process died (crash/OOM/killed); drives the shell's sad-tab state.
pub const CEF_REQUEST_HANDLER_ON_RENDER_PROCESS_TERMINATED_OFFSET: usize = 112;
/// `sizeof(cef_resource_request_handler_t)` for pinned Linux CEF 149.
pub const CEF_RESOURCE_REQUEST_HANDLER_SIZE: usize = 104;
/// `offsetof(cef_resource_request_handler_t, on_before_resource_load)`.
pub const CEF_RESOURCE_REQUEST_HANDLER_ON_BEFORE_RESOURCE_LOAD_OFFSET: usize = 48;
const CEF_PAGE_TEXT_BEACON_PREFIX: &str = "https://mde-page-text.invalid/capture/";
const CEF_PAGE_TEXT_BEACON_LEGACY_PREFIX: &str = "mde-page-text://capture/";
const CEF_PAGE_TEXT_BEACON_MAX_BYTES: u32 = 8 * 1024;
const CEF_PAGE_SCRAPE_BEACON_PREFIX: &str = "https://mde-page-scrape.invalid/capture/";
const CEF_PAGE_SCRAPE_BEACON_MAX_BYTES: usize = 32 * 1024;
const CEF_PASSKEY_BEACON_PREFIX: &str = "https://mde-passkey.invalid/request/";
const CEF_PASSKEY_BEACON_MAX_BYTES: usize = 8 * 1024;
const CEF_MEDIA_METADATA_BEACON_PREFIX: &str = "https://mde-media.invalid/metadata/";
const CEF_MEDIA_METADATA_BEACON_MAX_BYTES: usize = 8 * 1024;
/// Login-capture beacon: the page-side [`scripts::login_capture_script`] posts a
/// submitted login's JSON here; the resource-request handler intercepts + cancels it
/// (never hits the network — creds stay in the sandbox), mirroring the passkey beacon.
const CEF_LOGIN_BEACON_PREFIX: &str = "https://mde-login.invalid/capture/";
const CEF_LOGIN_BEACON_MAX_BYTES: usize = 8 * 1024;
/// `sizeof(cef_print_handler_t)` for pinned Linux CEF 149.
pub const CEF_PRINT_HANDLER_SIZE: usize = 88;
/// `offsetof(cef_print_handler_t, on_print_dialog)`.
pub const CEF_PRINT_HANDLER_ON_PRINT_DIALOG_OFFSET: usize = 56;
/// `offsetof(cef_print_handler_t, on_print_job)`.
pub const CEF_PRINT_HANDLER_ON_PRINT_JOB_OFFSET: usize = 64;
/// `offsetof(cef_print_handler_t, get_pdf_paper_size)`.
pub const CEF_PRINT_HANDLER_GET_PDF_PAPER_SIZE_OFFSET: usize = 80;
/// `sizeof(cef_browser_t)` for pinned Linux CEF 149.
pub const CEF_BROWSER_SIZE: usize = 208;
/// `offsetof(cef_browser_t, get_host)`.
pub const CEF_BROWSER_GET_HOST_OFFSET: usize = 48;
/// `offsetof(cef_browser_t, can_go_back)`.
pub const CEF_BROWSER_CAN_GO_BACK_OFFSET: usize = 56;
/// `offsetof(cef_browser_t, go_back)`.
pub const CEF_BROWSER_GO_BACK_OFFSET: usize = 64;
/// `offsetof(cef_browser_t, can_go_forward)`.
pub const CEF_BROWSER_CAN_GO_FORWARD_OFFSET: usize = 72;
/// `offsetof(cef_browser_t, go_forward)`.
pub const CEF_BROWSER_GO_FORWARD_OFFSET: usize = 80;
/// `offsetof(cef_browser_t, reload)`.
pub const CEF_BROWSER_RELOAD_OFFSET: usize = 96;
/// `offsetof(cef_browser_t, stop_load)`.
pub const CEF_BROWSER_STOP_LOAD_OFFSET: usize = 112;
/// `offsetof(cef_browser_t, get_main_frame)`.
pub const CEF_BROWSER_GET_MAIN_FRAME_OFFSET: usize = 152;
/// `cef_frame_t` edit-command offsets (base 40 + field*8): undo=1, redo=2, cut=3,
/// copy=4, paste=5, del=7, select_all=8. Each is `void method(self)`.
pub const CEF_FRAME_UNDO_OFFSET: usize = 48;
/// `offsetof(cef_frame_t, redo)`.
pub const CEF_FRAME_REDO_OFFSET: usize = 56;
/// `offsetof(cef_frame_t, cut)`.
pub const CEF_FRAME_CUT_OFFSET: usize = 64;
/// `offsetof(cef_frame_t, copy)`.
pub const CEF_FRAME_COPY_OFFSET: usize = 72;
/// `offsetof(cef_frame_t, paste)`.
pub const CEF_FRAME_PASTE_OFFSET: usize = 80;
/// `offsetof(cef_frame_t, del)`.
pub const CEF_FRAME_DELETE_OFFSET: usize = 96;
/// `offsetof(cef_frame_t, select_all)`.
pub const CEF_FRAME_SELECT_ALL_OFFSET: usize = 104;
/// `offsetof(cef_frame_t, get_text)` — native visible-text extraction.
pub const CEF_FRAME_GET_TEXT_OFFSET: usize = 128;
/// `sizeof(cef_browser_host_t)` for pinned Linux CEF 149.
pub const CEF_BROWSER_HOST_SIZE: usize = 592;
/// `offsetof(cef_browser_host_t, close_browser)`.
pub const CEF_BROWSER_HOST_CLOSE_BROWSER_OFFSET: usize = 48;
/// `offsetof(cef_browser_host_t, set_focus)`.
pub const CEF_BROWSER_HOST_SET_FOCUS_OFFSET: usize = 72;
/// `offsetof(cef_browser_host_t, set_zoom_level)` — native page zoom (field 15,
/// reconciled against close_browser=48 and set_focus=72).
pub const CEF_BROWSER_HOST_SET_ZOOM_LEVEL_OFFSET: usize = 160;
/// `offsetof(cef_browser_host_t, download_image)` — fetch an image URL through
/// the engine's connection (field 18, between set_zoom_level=160 and find=208).
/// Signature `void(self, const cef_string_t* image_url, int is_favicon,
/// uint32_t max_image_size, int bypass_cache, cef_download_image_callback_t*)`.
pub const CEF_BROWSER_HOST_DOWNLOAD_IMAGE_OFFSET: usize = 184;
/// `offsetof(cef_browser_host_t, find)` — native find-in-page (field 21).
pub const CEF_BROWSER_HOST_FIND_OFFSET: usize = 208;
/// `offsetof(cef_browser_host_t, stop_finding)` (field 22).
pub const CEF_BROWSER_HOST_STOP_FINDING_OFFSET: usize = 216;
/// `offsetof(cef_browser_host_t, was_resized)`.
pub const CEF_BROWSER_HOST_WAS_RESIZED_OFFSET: usize = 304;
/// `offsetof(cef_browser_host_t, invalidate)`.
pub const CEF_BROWSER_HOST_INVALIDATE_OFFSET: usize = 328;
/// `offsetof(cef_browser_host_t, send_key_event)`.
pub const CEF_BROWSER_HOST_SEND_KEY_EVENT_OFFSET: usize = 344;
/// `offsetof(cef_browser_host_t, send_mouse_click_event)`.
pub const CEF_BROWSER_HOST_SEND_MOUSE_CLICK_EVENT_OFFSET: usize = 352;
/// `offsetof(cef_browser_host_t, send_mouse_move_event)`.
pub const CEF_BROWSER_HOST_SEND_MOUSE_MOVE_EVENT_OFFSET: usize = 360;
/// `offsetof(cef_browser_host_t, send_mouse_wheel_event)`.
pub const CEF_BROWSER_HOST_SEND_MOUSE_WHEEL_EVENT_OFFSET: usize = 368;
/// `offsetof(cef_browser_host_t, print)` — field 19 of the pinned CEF 149
/// `cef_browser_host_t` vtable (`40 + 19*8`). Corrected from a stale 504 (field 58,
/// which is `set_accessibility_state`) that silently made `PrintPage` call the wrong
/// host method on live CEF — verified against `/opt/mde/cef/include/capi/cef_browser_capi.h`.
pub const CEF_BROWSER_HOST_PRINT_OFFSET: usize = 192;
/// `offsetof(cef_browser_host_t, print_to_pdf)` — field 20 (`40 + 20*8`). Corrected
/// from a stale 512 (field 59, `set_auto_resize_enabled`) that made `SavePdf` call the
/// wrong host method. Same ground-truth header.
pub const CEF_BROWSER_HOST_PRINT_TO_PDF_OFFSET: usize = 200;
/// `offsetof(cef_browser_host_t, set_audio_muted)`.
pub const CEF_BROWSER_HOST_SET_AUDIO_MUTED_OFFSET: usize = 520;
/// `offsetof(cef_browser_host_t, is_audio_muted)`.
pub const CEF_BROWSER_HOST_IS_AUDIO_MUTED_OFFSET: usize = 528;
/// `offsetof(cef_browser_host_t, ime_set_composition)` — field 47 of the pinned
/// CEF 149 vtable (base 40 + 47*8 = 416). Confirmed via `offsetof` against the
/// pinned header `149.0.6+g0d0eeb6+chromium-149.0.7827.201`, cross-checked by the
/// bracketing anchors `find`=208 (field 21) and `set_audio_muted`=520 (field 60).
/// Signature `void(self, const cef_string_t* text, size_t underlines_count,
/// const cef_composition_underline_t* underlines,
/// const cef_range_t* replacement_range, const cef_range_t* selection_range)`.
pub const CEF_BROWSER_HOST_IME_SET_COMPOSITION_OFFSET: usize = 416;
/// `offsetof(cef_browser_host_t, ime_commit_text)` — field 48 (40 + 48*8 = 424),
/// confirmed via `offsetof`. Signature `void(self, const cef_string_t* text,
/// const cef_range_t* replacement_range, int relative_cursor_pos)`.
pub const CEF_BROWSER_HOST_IME_COMMIT_TEXT_OFFSET: usize = 424;
/// `offsetof(cef_browser_host_t, ime_finish_composing_text)` — field 49
/// (40 + 49*8 = 432), confirmed via `offsetof`. Signature
/// `void(self, int keep_selection)`.
pub const CEF_BROWSER_HOST_IME_FINISH_COMPOSING_OFFSET: usize = 432;
/// `sizeof(cef_frame_t)` for pinned Linux CEF 149.
pub const CEF_FRAME_SIZE: usize = 248;
/// `offsetof(cef_frame_t, load_url)`.
pub const CEF_FRAME_LOAD_URL_OFFSET: usize = 144;
/// `offsetof(cef_frame_t, execute_java_script)`.
pub const CEF_FRAME_EXECUTE_JAVA_SCRIPT_OFFSET: usize = 152;
/// `sizeof(cef_request_t)` for pinned Linux CEF 149.
pub const CEF_REQUEST_SIZE: usize = 216;
/// `offsetof(cef_request_t, get_url)`.
pub const CEF_REQUEST_GET_URL_OFFSET: usize = 48;
/// `offsetof(cef_request_t, set_header_by_name)` for pinned Linux CEF 149.
///
/// `set_header_by_name` is index 13 of the `_cef_request_t` fn-ptr block that
/// follows the 40-byte `cef_base_ref_counted_t`: base 40 + 13*8 = 144. The order
/// of the classic `cef_request_capi.h` core (indices 0..14) — is_read_only(0),
/// get_url(1), set_url(2), get_method(3), set_method(4), set_referrer(5),
/// get_referrer_url(6), get_referrer_policy(7), get_post_data(8), set_post_data(9),
/// get_header_map(10), set_header_map(11), get_header_by_name(12),
/// set_header_by_name(13), set(14) — is ABI-frozen; CEF only appends new methods
/// after the last (get_identifier, index 21). Cross-checked by the two in-crate
/// anchors that pin the SAME struct: `CEF_REQUEST_GET_URL_OFFSET`=48 fixes index 1
/// against base 40, and `CEF_REQUEST_SIZE`=216 = 40 + 22*8 fixes the method count
/// at exactly 22 (indices 0..21) — both consistent only with this layout.
/// Signature `void(self, const cef_string_t* name, const cef_string_t* value,
/// int overwrite)`.
pub const CEF_REQUEST_SET_HEADER_BY_NAME_OFFSET: usize = 144;
/// `sizeof(cef_callback_t)` for pinned Linux CEF 149.
pub const CEF_CALLBACK_SIZE: usize = 56;
/// `offsetof(cef_callback_t, cont)`.
pub const CEF_CALLBACK_CONT_OFFSET: usize = 40;
/// `offsetof(cef_callback_t, cancel)`.
pub const CEF_CALLBACK_CANCEL_OFFSET: usize = 48;
/// `sizeof(cef_pdf_print_callback_t)` for pinned Linux CEF 149.
pub const CEF_PDF_PRINT_CALLBACK_SIZE: usize = 48;
/// `offsetof(cef_pdf_print_callback_t, on_pdf_print_finished)`.
pub const CEF_PDF_PRINT_CALLBACK_ON_FINISHED_OFFSET: usize = 40;
/// `sizeof(cef_download_image_callback_t)` for pinned Linux CEF 149 (base 40 +
/// one fn ptr). Numerically equal to `CEF_PDF_PRINT_CALLBACK_SIZE` — both are
/// single-method one-shot callbacks — so `callback_size` needs no new arm.
pub const CEF_DOWNLOAD_IMAGE_CALLBACK_SIZE: usize = 48;
/// `offsetof(cef_download_image_callback_t, on_download_image_finished)`.
/// Signature `void(self, const cef_string_t* image_url, int http_status_code,
/// cef_image_t* image)` (image may be NULL on failure).
pub const CEF_DOWNLOAD_IMAGE_CALLBACK_ON_FINISHED_OFFSET: usize = 40;
/// `offsetof(cef_image_t, get_as_png)` for pinned Linux CEF 149 (field 11).
/// Signature `cef_binary_value_t* get_as_png(self, float scale_factor,
/// int with_transparency, int* pixel_width, int* pixel_height)`.
pub const CEF_IMAGE_GET_AS_PNG_OFFSET: usize = 128;
/// `offsetof(cef_binary_value_t, get_size)` for pinned Linux CEF 149 (field 6).
pub const CEF_BINARY_VALUE_GET_SIZE_OFFSET: usize = 88;
/// `offsetof(cef_binary_value_t, get_data)` for pinned Linux CEF 149 (field 7).
/// Signature `size_t get_data(self, void* buffer, size_t buffer_size,
/// size_t data_offset)`.
pub const CEF_BINARY_VALUE_GET_DATA_OFFSET: usize = 96;
/// `sizeof(cef_mouse_event_t)` for pinned Linux CEF 149.
pub const CEF_MOUSE_EVENT_SIZE: usize = 12;
/// `offsetof(cef_mouse_event_t, x)`.
pub const CEF_MOUSE_EVENT_X_OFFSET: usize = 0;
/// `offsetof(cef_mouse_event_t, y)`.
pub const CEF_MOUSE_EVENT_Y_OFFSET: usize = 4;
/// `offsetof(cef_mouse_event_t, modifiers)`.
pub const CEF_MOUSE_EVENT_MODIFIERS_OFFSET: usize = 8;
/// `sizeof(cef_key_event_t)` for pinned Linux CEF 149.
pub const CEF_KEY_EVENT_SIZE: usize = 40;
/// `offsetof(cef_key_event_t, type)`.
pub const CEF_KEY_EVENT_TYPE_OFFSET: usize = 8;
/// `offsetof(cef_key_event_t, modifiers)`.
pub const CEF_KEY_EVENT_MODIFIERS_OFFSET: usize = 12;
/// `offsetof(cef_key_event_t, windows_key_code)`.
pub const CEF_KEY_EVENT_WINDOWS_KEY_CODE_OFFSET: usize = 16;
/// `offsetof(cef_key_event_t, native_key_code)`.
pub const CEF_KEY_EVENT_NATIVE_KEY_CODE_OFFSET: usize = 20;
/// `offsetof(cef_key_event_t, is_system_key)`.
pub const CEF_KEY_EVENT_IS_SYSTEM_KEY_OFFSET: usize = 24;
/// `offsetof(cef_key_event_t, character)`.
pub const CEF_KEY_EVENT_CHARACTER_OFFSET: usize = 28;
/// `offsetof(cef_key_event_t, unmodified_character)`.
pub const CEF_KEY_EVENT_UNMODIFIED_CHARACTER_OFFSET: usize = 30;
/// `offsetof(cef_key_event_t, focus_on_editable_field)`.
pub const CEF_KEY_EVENT_FOCUS_ON_EDITABLE_FIELD_OFFSET: usize = 32;

const BASE_SIZE_OFFSET: usize = 0;
const BASE_ADD_REF_OFFSET: usize = 8;
const BASE_RELEASE_OFFSET: usize = 16;
const BASE_HAS_ONE_REF_OFFSET: usize = 24;
const BASE_HAS_AT_LEAST_ONE_REF_OFFSET: usize = 32;
/// `sizeof(cef_rect_t)` for pinned Linux CEF 149.
pub const CEF_RECT_SIZE: usize = 16;
const PET_VIEW: c_int = 0;
/// The engine's popup widget paint (`<select>` dropdown / autocomplete list).
const PET_POPUP: c_int = 1;
const MBT_LEFT: c_int = 0;
const MBT_MIDDLE: c_int = 1;
const MBT_RIGHT: c_int = 2;
const KEYEVENT_RAWKEYDOWN: c_int = 0;
const KEYEVENT_KEYUP: c_int = 2;
const KEYEVENT_CHAR: c_int = 3;
const RV_CANCEL: c_int = 0;
const RV_CONTINUE: c_int = 1;
const RV_CONTINUE_ASYNC: c_int = 2;
const RESOURCE_OTHER: u8 = 255;
/// Cap CEF callbacks held while waiting for shell resource verdicts. The shell
/// answers immediately in normal operation, so hitting this is backpressure or a
/// wedged peer; fail closed instead of retaining unbounded live callbacks.
const MAX_PENDING_RESOURCE_REQUESTS: usize = 128;
/// Cap CEF permission-prompt callbacks held while waiting for shell answers. The
/// preview client has the same visible queue bound; the engine also needs its own
/// fail-closed cap for a wedged shell.
const MAX_PENDING_PERMISSION_PROMPTS: usize = 16;
/// Cap CEF beforeunload callbacks held while waiting for shell answers. Overflow
/// is answered as "stay/cancel" so navigation never proceeds on backpressure.
const MAX_PENDING_BEFORE_UNLOAD_DIALOGS: usize = 16;
/// Cap native CEF text visitor callbacks retained while `cef_frame_t::get_text`
/// replies asynchronously. The JS beacon remains a fallback; do not retain
/// unbounded visitor blocks if the renderer stops answering text requests.
const MAX_PENDING_PAGE_TEXT_VISITORS: usize = 32;
const EVENTFLAG_SHIFT_DOWN: c_int = 2;
const EVENTFLAG_CONTROL_DOWN: c_int = 4;
const EVENTFLAG_ALT_DOWN: c_int = 8;
const EVENTFLAG_LEFT_MOUSE_BUTTON: c_int = 16;
const EVENTFLAG_MIDDLE_MOUSE_BUTTON: c_int = 32;
const EVENTFLAG_RIGHT_MOUSE_BUTTON: c_int = 64;
const EVENTFLAG_COMMAND_DOWN: c_int = 128;

/// Result of a bounded windowless browser probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefBrowserProbe {
    /// URL passed to CEF.
    pub url: String,
    /// Requested view width.
    pub width: u32,
    /// Requested view height.
    pub height: u32,
    /// Number of browser-created callbacks seen.
    pub created: usize,
    /// Number of view paint callbacks seen.
    pub paints: usize,
    /// Last paint width.
    pub last_paint_width: i32,
    /// Last paint height.
    pub last_paint_height: i32,
}

/// Result of a bounded page-text smoke probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefTextProbe {
    /// Browser paint probe details.
    pub browser: CefBrowserProbe,
    /// Text marker that had to appear in visible page text.
    pub expected: String,
    /// Bytes captured from visible page text.
    pub text_bytes: usize,
}

impl CefBrowserProbe {
    /// Operator-facing status line.
    #[must_use]
    pub fn status_line(&self) -> String {
        format!(
            "CEF_BROWSER_PAINT_READY url={} view={}x{} created={} paints={} last_paint={}x{}",
            self.url,
            self.width,
            self.height,
            self.created,
            self.paints,
            self.last_paint_width,
            self.last_paint_height
        )
    }
}

impl CefTextProbe {
    /// Operator-facing status line.
    #[must_use]
    pub fn status_line(&self) -> String {
        format!(
            "CEF_TEXT_PROBE_READY url={} view={}x{} created={} paints={} marker_bytes={} text_bytes={}",
            self.browser.url,
            self.browser.width,
            self.browser.height,
            self.browser.created,
            self.browser.paints,
            self.expected.len(),
            self.text_bytes
        )
    }
}

/// Error from creating or pumping a windowless CEF browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefBrowserError {
    /// URL contained an interior NUL-equivalent UTF-16 issue.
    BadUrl,
    /// CEF returned a null browser pointer.
    CreateReturnedNull,
    /// No paint callback arrived before the bounded deadline.
    TimedOut {
        /// Browser-created callbacks observed.
        created: usize,
        /// Paint callbacks observed.
        paints: usize,
    },
    /// Browser painted but the expected text marker was not observed.
    TextProbeMissing {
        /// Browser-created callbacks observed.
        created: usize,
        /// Paint callbacks observed.
        paints: usize,
        /// Last captured text byte count, if any page text arrived.
        text_bytes: usize,
    },
    /// Creating or publishing through the BOOKMARKS-6 frame sink failed.
    Offscreen(String),
}

impl fmt::Display for CefBrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadUrl => write!(f, "URL cannot be represented as a CEF string"),
            Self::CreateReturnedNull => {
                write!(f, "cef_browser_host_create_browser_sync returned null")
            }
            Self::TimedOut { created, paints } => write!(
                f,
                "timed out waiting for CEF paint callback (created={created} paints={paints})"
            ),
            Self::TextProbeMissing {
                created,
                paints,
                text_bytes,
            } => write!(
                f,
                "timed out waiting for CEF text marker (created={created} paints={paints} text_bytes={text_bytes})"
            ),
            Self::Offscreen(err) => write!(f, "CEF offscreen frame sink failed: {err}"),
        }
    }
}

impl std::error::Error for CefBrowserError {}

/// Create one offscreen browser and pump until the first view paint arrives.
///
/// # Errors
/// Returns [`CefBrowserError`] when CEF does not create the browser or no paint
/// arrives before `timeout`.
pub fn run_windowless_browser_probe(
    abi: &CefAbi,
    url: &str,
    width: u32,
    height: u32,
    timeout: Duration,
) -> Result<CefBrowserProbe, CefBrowserError> {
    run_windowless_browser_probe_with_stream(abi, url, width, height, timeout, None)
}

/// Create one offscreen browser, optionally attach its frames to `stream`, and
/// pump until the first view paint arrives.
///
/// # Errors
/// Returns [`CefBrowserError`] when CEF does not create the browser, frame-sink
/// setup fails, or no paint arrives before `timeout`.
pub fn run_windowless_browser_probe_with_stream(
    abi: &CefAbi,
    url: &str,
    width: u32,
    height: u32,
    timeout: Duration,
    stream: Option<&UnixStream>,
) -> Result<CefBrowserProbe, CefBrowserError> {
    let window_info = CefWindowInfo::windowless(width, height);
    let browser_settings = CefBrowserSettings::windowless(30);
    let url = CefStringOwned::new(url)?;
    let callbacks = CefBrowserCallbacks::new(
        width,
        height,
        stream,
        abi.string_userfree_utf16_free(),
        abi.string_list_size(),
        abi.string_list_value(),
    )?;

    let browser = abi.create_browser_sync(
        window_info.as_ptr(),
        callbacks.client_ptr(),
        url.as_ptr(),
        browser_settings.as_ptr(),
    );
    if browser.is_null() {
        abi.shutdown();
        return Err(CefBrowserError::CreateReturnedNull);
    }
    notify_browser_view_ready(browser);

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        abi.do_message_loop_work();
        if callbacks.paints() > 0 {
            let probe = CefBrowserProbe {
                url: url.text().to_owned(),
                width,
                height,
                created: callbacks.created(),
                paints: callbacks.paints(),
                last_paint_width: callbacks.last_paint_width(),
                last_paint_height: callbacks.last_paint_height(),
            };
            // Close the browser and wait for CEF's on_before_close signal before
            // cef_shutdown. A fixed short drain still left render-once racing
            // teardown after a correct paint on the farm.
            close_browser_and_wait(abi, browser, &callbacks);
            abi.shutdown();
            return Ok(probe);
        }
        thread::sleep(Duration::from_millis(10));
    }

    close_browser_and_wait(abi, browser, &callbacks);
    abi.shutdown();
    Err(CefBrowserError::TimedOut {
        created: callbacks.created(),
        paints: callbacks.paints(),
    })
}

/// Create a windowless browser, request visible page text, and require a marker.
///
/// This is the runtime smoke primitive used for live WebExtension proof: a test
/// extension can inject a visible marker into a page, and this probe only passes
/// when CEF paints and the existing page-text beacon observes that marker.
///
/// # Errors
/// Returns [`CefBrowserError`] when CEF does not paint or the marker is absent
/// before the bounded deadline.
pub fn run_windowless_text_probe(
    abi: &CefAbi,
    url: &str,
    width: u32,
    height: u32,
    timeout: Duration,
    expected: &str,
) -> Result<CefTextProbe, CefBrowserError> {
    const TEXT_PROBE_ID: u64 = u64::MAX - 7;
    const TEXT_PROBE_MAX_BYTES: u32 = 16 * 1024;

    let (helper, shell) = UnixStream::pair()?;
    helper.set_nonblocking(true)?;
    shell.set_nonblocking(true)?;

    let window_info = CefWindowInfo::windowless(width, height);
    let browser_settings = CefBrowserSettings::windowless(30);
    let url = CefStringOwned::new(url)?;
    let callbacks = CefBrowserCallbacks::new(
        width,
        height,
        Some(&helper),
        abi.string_userfree_utf16_free(),
        abi.string_list_size(),
        abi.string_list_value(),
    )?;

    let browser = abi.create_browser_sync(
        window_info.as_ptr(),
        callbacks.client_ptr(),
        url.as_ptr(),
        browser_settings.as_ptr(),
    );
    if browser.is_null() {
        abi.shutdown();
        return Err(CefBrowserError::CreateReturnedNull);
    }
    notify_browser_view_ready(browser);

    let deadline = Instant::now() + timeout;
    let probe_started = Instant::now();
    let mut rbuf = Vec::new();
    let mut first_paint = None;
    let mut last_text_bytes = 0;
    let mut saw_loading = false;
    let mut last_text_request = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        abi.do_message_loop_work();
        match sock::recv(&shell) {
            Ok(RecvOutcome::Data { bytes, .. }) => {
                rbuf.extend_from_slice(&bytes);
                if let Some(text) = drain_page_text_events(&mut rbuf, TEXT_PROBE_ID) {
                    last_text_bytes = text.len();
                    if text.contains(expected) {
                        let browser_probe = first_paint.unwrap_or(CefBrowserProbe {
                            url: url.text().to_owned(),
                            width,
                            height,
                            created: callbacks.created(),
                            paints: callbacks.paints(),
                            last_paint_width: callbacks.last_paint_width(),
                            last_paint_height: callbacks.last_paint_height(),
                        });
                        close_browser(browser);
                        for _ in 0..8 {
                            abi.do_message_loop_work();
                            thread::sleep(Duration::from_millis(4));
                        }
                        abi.shutdown();
                        return Ok(CefTextProbe {
                            browser: browser_probe,
                            expected: expected.to_owned(),
                            text_bytes: last_text_bytes,
                        });
                    }
                }
            }
            Ok(RecvOutcome::WouldBlock) => {}
            Ok(RecvOutcome::Eof) | Err(_) => break,
        }
        if first_paint.is_none() && callbacks.paints() > 0 {
            first_paint = Some(CefBrowserProbe {
                url: url.text().to_owned(),
                width,
                height,
                created: callbacks.created(),
                paints: callbacks.paints(),
                last_paint_width: callbacks.last_paint_width(),
                last_paint_height: callbacks.last_paint_height(),
            });
        }
        if callbacks.loading() {
            saw_loading = true;
        }
        let load_ready = !callbacks.loading()
            && (saw_loading || probe_started.elapsed() >= Duration::from_millis(750));
        if first_paint.is_some()
            && load_ready
            && last_text_request.elapsed() >= Duration::from_millis(250)
        {
            request_page_text(
                browser,
                &callbacks.state,
                TEXT_PROBE_ID,
                TEXT_PROBE_MAX_BYTES,
            );
            last_text_request = Instant::now();
        }
        thread::sleep(Duration::from_millis(8));
    }

    // Same live-browser-before-shutdown discipline as the success path.
    close_browser_and_wait(abi, browser, &callbacks);
    abi.shutdown();
    Err(CefBrowserError::TextProbeMissing {
        created: callbacks.created(),
        paints: callbacks.paints(),
        text_bytes: last_text_bytes,
    })
}

/// Pump interval while the tab is actively loading, painting, or receiving
/// input. Unchanged from the original 125 Hz spin so input/paint latency is not
/// regressed while active.
const PUMP_ACTIVE: Duration = Duration::from_millis(8);
/// Pump interval once the tab has gone quiet. An idle tab no longer spins at
/// 125 Hz; it wakes ~4x/s (and immediately on any socket control frame, since
/// the loop waits with `poll()` on the session fd rather than a blind sleep).
/// This is also the passkey outbound-drain heartbeat, so it doubles as the idle
/// floor rather than adding a second timer.
const PUMP_IDLE: Duration = Duration::from_millis(250);
/// How long after the last activity a freshly-navigated document is still
/// treated as "settling", during which the per-context shims are re-applied (at
/// [`ShimInjector::SETTLE_INTERVAL`]) so a slow document commit is still covered
/// even though the pinned CEF ABI exposes no load/context-ready callback.
const SHIM_SETTLE: Duration = Duration::from_millis(1000);
/// Cadence of the passkey outbound-queue drain. The ceremony beacon/queue design
/// is inherently poll-based (a page-initiated `credentials.get()` enqueues a
/// request native must pick up), so a lightweight drain keeps running — but it no
/// longer recompiles the multi-KB bridge shim on every tick.
const PASSKEY_DRAIN_INTERVAL: Duration = Duration::from_millis(250);
/// Cadence for now-playing metadata probes. The script dedupes in-page before
/// beaconing, so an unchanged media page remains quiet while this poll runs.
const MEDIA_METADATA_POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// Cadence for a stronger OSR repaint nudge while waiting for the first frame
/// or while media is actively playing.
/// `invalidate(PET_VIEW)` alone can be too sparse on the pinned CEF 149 farm
/// runtime; a same-size `was_resized` pulse reliably wakes the compositor path,
/// but stops once a non-media page has painted so settled static pages still idle.
const MEDIA_VIEW_RESIZE_NUDGE_INTERVAL: Duration = Duration::from_millis(250);
/// Emergency/privacy override: when set to `1`/`true`/`yes`/`on`, CEF keeps the
/// legacy best-effort JS WebRTC block. The operational default is enabled WebRTC
/// with CEF's real IP-handling policy plus the native media permission callback.
const CEF_WEBRTC_BLOCK_ENV: &str = "MDE_CEF_WEBRTC_BLOCKED";

/// Whether a newly-loaded CEF page is still in the bounded media-discovery
/// window. This lets the metadata/audio probes catch quiet autoplay or muted
/// video before the OSR pump backs off.
fn media_discovery_active(
    idle_for: Duration,
    awaiting_first_paint: bool,
    active_media: bool,
) -> bool {
    awaiting_first_paint || active_media || idle_for < SHIM_SETTLE
}

/// perf-6: pick the next pump/poll interval from how active the tab is.
///
/// While awaiting the first paint, playing media, or still inside the bounded
/// post-navigation media-discovery window the tab is "active" and pumped at
/// [`PUMP_ACTIVE`]. Sustained quiet with no active media backs off to
/// [`PUMP_IDLE`] so an idle tab stops spinning at 125 Hz. Input latency is
/// preserved because the loop waits on the session fd with `poll()`, which
/// returns immediately when a control frame lands regardless of the interval.
fn pump_interval(idle_for: Duration, awaiting_first_paint: bool, active_media: bool) -> Duration {
    if media_discovery_active(idle_for, awaiting_first_paint, active_media) {
        PUMP_ACTIVE
    } else {
        PUMP_IDLE
    }
}

/// Whether the windowless CEF view should be explicitly invalidated on this pump.
///
/// Static pages paint naturally after the initial resize/invalidate. Media can
/// advance before CEF reports audio or now-playing metadata, and on the farm CEF
/// 149 windowless runtime does not always schedule an OSR paint for that alone.
/// During the bounded discovery window, nudge CEF's view invalidation at the
/// active cadence; settled non-media pages still back off.
fn should_invalidate_view(media_discovery_active: bool) -> bool {
    media_discovery_active
}

fn should_resize_view(media_discovery_active: bool, since_last_nudge: Duration) -> bool {
    media_discovery_active && since_last_nudge >= MEDIA_VIEW_RESIZE_NUDGE_INTERVAL
}

/// browser-8: decides when to (re)inject the per-context document shims (optional
/// WebRTC block + passkey/login/autoplay bridges) so they land once per navigation
/// generation instead of on a blind 250 ms timer.
///
/// The pinned CEF ABI exposes no `OnContextCreated`/load-end callback, only an
/// `is_navigation` flag on the resource handler, so navigation is modelled as a
/// monotonic `generation` counter. A new generation always injects once. While
/// that generation is still `settling` (document committing / first paints
/// arriving) it re-injects at most once per [`Self::SETTLE_INTERVAL`] so a slow
/// commit is covered. Once the context is stable it never re-injects — the
/// per-document WebRTC `MutationObserver` keeps new subframes covered on its own
/// when the optional block is enabled.
#[derive(Debug, Default)]
struct ShimInjector {
    injected_generation: Option<u64>,
    last_inject: Option<Instant>,
}

fn cef_webrtc_blocked_from_env() -> bool {
    let value = std::env::var(CEF_WEBRTC_BLOCK_ENV).ok();
    cef_webrtc_blocked_from_env_value(value.as_deref())
}

fn cef_webrtc_blocked_from_env_value(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

impl ShimInjector {
    /// Bounded re-inject cadence while a fresh document is settling.
    const SETTLE_INTERVAL: Duration = Duration::from_millis(250);

    fn new() -> Self {
        Self::default()
    }

    /// Returns true when the shims should be injected this tick, recording the
    /// decision so the same stable context is never re-injected on a timer.
    fn should_inject(&mut self, generation: u64, settling: bool, now: Instant) -> bool {
        if self.injected_generation != Some(generation) {
            self.injected_generation = Some(generation);
            self.last_inject = Some(now);
            return true;
        }
        if settling {
            let due = self
                .last_inject
                .is_none_or(|last| now.duration_since(last) >= Self::SETTLE_INTERVAL);
            if due {
                self.last_inject = Some(now);
                return true;
            }
        }
        false
    }
}

/// Wait up to `timeout` for the session fd to become readable, so a control
/// frame wakes the loop immediately instead of after a fixed sleep. `poll()`
/// also reports `POLLHUP`/`POLLERR`, so a closed peer wakes us to observe EOF.
/// The return value is intentionally ignored: the next `sock::recv` classifies
/// data/would-block/EOF.
fn wait_for_readable(fd: RawFd, timeout: Duration) {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = c_int::try_from(timeout.as_millis())
        .unwrap_or(c_int::MAX)
        .max(1);
    // SAFETY: `pfd` is a single valid `pollfd` for the borrowed session fd; the
    // call only reads `events`/`fd` and writes `revents`.
    unsafe {
        libc::poll(std::ptr::addr_of_mut!(pfd), 1, millis);
    }
}

/// Serve one CEF tab over the BOOKMARKS-6 session socket until the shell closes
/// the socket. This currently supports initial load + frame publication; decoded
/// control frames are drained so the stream stays aligned while navigation/input
/// callbacks are added in the next slice.
///
/// # Errors
/// Returns [`CefBrowserError`] if browser creation or frame-sink setup fails.
pub fn run_windowless_tab(
    abi: &CefAbi,
    url: &str,
    width: u32,
    height: u32,
    stream: &UnixStream,
) -> Result<CefBrowserProbe, CefBrowserError> {
    stream.set_nonblocking(true)?;
    let window_info = CefWindowInfo::windowless(width, height);
    let browser_settings = CefBrowserSettings::windowless(30);
    let url = CefStringOwned::new(url)?;
    let callbacks = CefBrowserCallbacks::new(
        width,
        height,
        Some(stream),
        abi.string_userfree_utf16_free(),
        abi.string_list_size(),
        abi.string_list_value(),
    )?;

    let browser = abi.create_browser_sync(
        window_info.as_ptr(),
        callbacks.client_ptr(),
        url.as_ptr(),
        browser_settings.as_ptr(),
    );
    if browser.is_null() {
        abi.shutdown();
        return Err(CefBrowserError::CreateReturnedNull);
    }
    notify_browser_view_ready(browser);
    // Best-effort earliest document-shim injection, ahead of the poll loop below.
    inject_context_shims(browser, &callbacks.state);

    let mut first_paint = None;
    let started = Instant::now();
    let mut rbuf = Vec::new();
    let fd = stream.as_raw_fd();

    // browser-8: inject the per-context document shims (optional WebRTC block +
    // passkey/login/autoplay bridges) once per navigation generation instead of on
    // a fixed 250 ms timer.
    let mut shims = ShimInjector::new();
    let mut last_nav = callbacks.navigations();
    let mut last_passkey_drain = Instant::now();
    let mut last_media_metadata_poll = Instant::now()
        .checked_sub(MEDIA_METADATA_POLL_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_media_view_resize = Instant::now()
        .checked_sub(MEDIA_VIEW_RESIZE_NUDGE_INTERVAL)
        .unwrap_or_else(Instant::now);

    // perf-6: adaptive pump. `last_activity` tracks the last paint / control
    // frame / navigation so the pump runs fast while the tab is active and backs
    // off when idle instead of spinning at 125 Hz forever.
    let mut last_activity = Instant::now();
    let mut prev_paints = callbacks.paints();

    loop {
        abi.do_message_loop_work();
        match sock::recv(stream) {
            Ok(RecvOutcome::Data { bytes, .. }) => {
                rbuf.extend_from_slice(&bytes);
                drain_control_frames(&mut rbuf, browser, &callbacks);
                last_activity = Instant::now();
            }
            Ok(RecvOutcome::WouldBlock) => {}
            Ok(RecvOutcome::Eof) => break,
            Err(_) => break,
        }

        let paints = callbacks.paints();
        if paints != prev_paints {
            prev_paints = paints;
            last_activity = Instant::now();
        }
        if first_paint.is_none() && paints > 0 {
            first_paint = Some(CefBrowserProbe {
                url: url.text().to_owned(),
                width,
                height,
                created: callbacks.created(),
                paints,
                last_paint_width: callbacks.last_paint_width(),
                last_paint_height: callbacks.last_paint_height(),
            });
        }

        let nav = callbacks.navigations();
        if nav != last_nav {
            last_nav = nav;
            last_activity = Instant::now();
        }

        let now = Instant::now();
        let idle_for = now.duration_since(last_activity);
        let awaiting_first_paint = first_paint.is_none();
        let active_media = callbacks.state.active_media();
        let media_probe_active =
            media_discovery_active(idle_for, awaiting_first_paint, active_media);
        if should_resize_view(
            media_probe_active,
            now.duration_since(last_media_view_resize),
        ) {
            resize_and_invalidate_browser_view(browser);
            last_media_view_resize = now;
        } else if should_invalidate_view(media_probe_active) {
            invalidate_browser_view(browser);
        }
        // A freshly-navigated document is still "settling" while awaiting the
        // first paint or within SHIM_SETTLE of the last activity; re-inject the
        // shims through the commit, then leave the stable context alone.
        let settling = awaiting_first_paint || idle_for < SHIM_SETTLE;
        if shims.should_inject(nav, settling, now) {
            inject_context_shims(browser, &callbacks.state);
        }
        // Keep draining page-initiated passkey ceremonies (cheap; no shim
        // recompile) — this is genuine outbound polling, not shim re-injection.
        if last_passkey_drain.elapsed() >= PASSKEY_DRAIN_INTERVAL {
            poll_passkey_drain(browser);
            last_passkey_drain = now;
        }
        if last_media_metadata_poll.elapsed() >= MEDIA_METADATA_POLL_INTERVAL {
            poll_media_metadata(browser);
            last_media_metadata_poll = now;
        }

        if awaiting_first_paint && started.elapsed() > Duration::from_secs(15) {
            abi.shutdown();
            return Err(CefBrowserError::TimedOut {
                created: callbacks.created(),
                paints: callbacks.paints(),
            });
        }

        wait_for_readable(
            fd,
            pump_interval(idle_for, awaiting_first_paint, active_media),
        );
    }

    let probe = first_paint.unwrap_or(CefBrowserProbe {
        url: url.text().to_owned(),
        width,
        height,
        created: callbacks.created(),
        paints: callbacks.paints(),
        last_paint_width: callbacks.last_paint_width(),
        last_paint_height: callbacks.last_paint_height(),
    });
    close_browser_and_wait(abi, browser, &callbacks);
    abi.shutdown();
    Ok(probe)
}

impl From<OffscreenError> for CefBrowserError {
    fn from(value: OffscreenError) -> Self {
        Self::Offscreen(value.to_string())
    }
}

impl From<std::io::Error> for CefBrowserError {
    fn from(value: std::io::Error) -> Self {
        Self::Offscreen(value.to_string())
    }
}

fn drain_control_frames(rbuf: &mut Vec<u8>, browser: *mut c_void, callbacks: &CefBrowserCallbacks) {
    loop {
        match wire::take_frame(rbuf) {
            Ok(Some(payload)) => {
                if let Ok(msg) = ControlMsg::decode(&payload) {
                    apply_control_frame(browser, callbacks, &msg);
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
}

fn drain_page_text_events(rbuf: &mut Vec<u8>, expected_id: u64) -> Option<String> {
    let mut latest = None;
    loop {
        match wire::take_frame(rbuf) {
            Ok(Some(payload)) => {
                if let Ok(EventMsg::PageText { id, text }) = EventMsg::decode(&payload) {
                    if id == expected_id {
                        latest = Some(text);
                    }
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    latest
}

fn apply_control_frame(browser: *mut c_void, callbacks: &CefBrowserCallbacks, msg: &ControlMsg) {
    match msg {
        ControlMsg::Load(url) => load_url(browser, url),
        ControlMsg::Reload => call_browser_void(browser, CEF_BROWSER_RELOAD_OFFSET),
        ControlMsg::Stop => call_browser_void(browser, CEF_BROWSER_STOP_LOAD_OFFSET),
        ControlMsg::Back => {
            if call_browser_bool(browser, CEF_BROWSER_CAN_GO_BACK_OFFSET) {
                call_browser_void(browser, CEF_BROWSER_GO_BACK_OFFSET);
            }
        }
        ControlMsg::Forward => {
            if call_browser_bool(browser, CEF_BROWSER_CAN_GO_FORWARD_OFFSET) {
                call_browser_void(browser, CEF_BROWSER_GO_FORWARD_OFFSET);
            }
        }
        ControlMsg::Resize { width, height } => {
            callbacks.resize(*width, *height);
            notify_browser_view_ready(browser);
        }
        ControlMsg::Input(event) => apply_input_event(browser, callbacks, event),
        ControlMsg::CosmeticFilters(css) => apply_cosmetic_filters(browser, css),
        ControlMsg::SetZoom { percent } => apply_page_zoom(browser, *percent),
        ControlMsg::FindInPage {
            query,
            backwards,
            find_next,
        } => apply_find_in_page(browser, query, *backwards, *find_next),
        ControlMsg::ClearFind => clear_find_in_page(browser),
        ControlMsg::EditCommand { command } => apply_edit_command(browser, *command),
        ControlMsg::SetAudioMuted { muted } => set_audio_muted(browser, *muted),
        ControlMsg::ToggleMediaPlayback => apply_media_playback_toggle(browser),
        ControlMsg::MediaTransport { action } => apply_media_transport(browser, *action),
        ControlMsg::SetAutoplayBlocked { blocked } => {
            apply_autoplay_blocked(browser, &callbacks.state, *blocked);
        }
        ControlMsg::SetForceDark { enabled } => apply_force_dark(browser, *enabled),
        ControlMsg::SetReaderMode { enabled } => apply_reader_mode(browser, *enabled),
        ControlMsg::SetUserScripts { enabled, bundle } => {
            apply_user_scripts(browser, *enabled, bundle);
        }
        ControlMsg::SetUserAgent { user_agent } => {
            // Store the override for the real HTTP `User-Agent:` header path
            // (on_before_resource_load), AND keep injecting the JS
            // `navigator.userAgent` shim for client-side sniffers.
            callbacks.state.set_user_agent_override(user_agent);
            apply_user_agent(browser, user_agent);
        }
        ControlMsg::SetDeviceProfile {
            profile,
            width,
            height,
            scale_percent,
            touch,
        } => apply_device_profile(browser, profile, *width, *height, *scale_percent, *touch),
        ControlMsg::SetSpellcheckHighlights { words } => {
            apply_spellcheck_highlights(browser, words);
        }
        ControlMsg::ApplySpellcheckCorrection { word, replacement } => {
            apply_spellcheck_correction(browser, word, replacement);
        }
        ControlMsg::ApplySpellcheckCorrectionAll { word, replacement } => {
            apply_spellcheck_correction_all(browser, word, replacement);
        }
        ControlMsg::ApplySpellcheckCorrectionAt {
            word,
            replacement,
            occurrence,
        } => {
            apply_spellcheck_correction_at(browser, word, replacement, *occurrence);
        }
        ControlMsg::PrintPage => print_page(browser),
        ControlMsg::SavePdf { path } => save_pdf(browser, callbacks, path),
        ControlMsg::RequestPageText { id, max_bytes } => {
            request_page_text(browser, &callbacks.state, *id, *max_bytes);
        }
        ControlMsg::RequestPageScrape {
            id,
            max_bytes,
            max_links,
            max_headings,
        } => {
            request_page_scrape(browser, *id, *max_bytes, *max_links, *max_headings);
        }
        ControlMsg::CompletePasskey { body } => complete_passkey(browser, body),
        ControlMsg::ResourceVerdict { id, allow } => callbacks.apply_resource_verdict(*id, *allow),
        ControlMsg::PermissionDecision { id, allow } => {
            callbacks.apply_permission_decision(*id, *allow);
        }
        ControlMsg::BeforeUnloadDecision { id, proceed } => {
            callbacks.apply_before_unload_decision(*id, *proceed);
        }
        ControlMsg::ImeSetComposition { text } => ime_set_composition(browser, text),
        ControlMsg::ImeCommitText { text } => ime_commit_text(browser, text),
        ControlMsg::ImeFinishComposition => ime_finish_composing(browser),
        ControlMsg::FillLogin {
            expected_host,
            username,
            password,
        } => fill_login(browser, &callbacks.state, expected_host, username, password),
    }
}

fn apply_input_event(browser: *mut c_void, callbacks: &CefBrowserCallbacks, event: &InputEvent) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    match event {
        InputEvent::PointerMoved { x, y } => {
            let (x, y) = callbacks.update_pointer(*x, *y);
            // Held-button flags make a press-drag a real drag (text selection,
            // scrollbar-thumb drag) instead of a plain hover.
            send_mouse_move(host, x, y, callbacks.held_button_flags(), false);
        }
        InputEvent::PointerButton {
            x,
            y,
            button,
            pressed,
            modifiers,
        } => {
            let (x, y) = callbacks.update_pointer(*x, *y);
            let click_count = if *pressed {
                set_host_focus(host, true);
                callbacks.register_press(*button, x, y)
            } else {
                callbacks.register_release(*button)
            };
            send_mouse_click(
                host,
                x,
                y,
                *button,
                *pressed,
                cef_modifiers(*modifiers) | callbacks.held_button_flags(),
                click_count,
            );
        }
        InputEvent::PointerGone => {
            let (x, y) = callbacks.pointer_position();
            send_mouse_move(host, x, y, callbacks.held_button_flags(), true);
        }
        InputEvent::Scroll {
            delta_x,
            delta_y,
            modifiers,
        } => {
            let (x, y) = callbacks.pointer_position();
            send_mouse_wheel(host, x, y, *delta_x, *delta_y, cef_modifiers(*modifiers));
        }
        InputEvent::Key {
            key,
            pressed,
            modifiers,
        } => {
            set_host_focus(host, true);
            send_key(host, *key, *pressed, *modifiers);
        }
        InputEvent::Text(text) => {
            set_host_focus(host, true);
            for unit in text.encode_utf16() {
                send_char(host, unit, Modifiers::default());
            }
        }
    }
}

struct CefWindowInfo {
    bytes: [u8; CEF_WINDOW_INFO_SIZE],
}

impl CefWindowInfo {
    fn windowless(width: u32, height: u32) -> Self {
        let mut info = Self {
            bytes: [0; CEF_WINDOW_INFO_SIZE],
        };
        info.put_usize(0, CEF_WINDOW_INFO_SIZE);
        info.put_i32(
            CEF_WINDOW_INFO_BOUNDS_OFFSET + 8,
            i32::try_from(width).unwrap_or(i32::MAX),
        );
        info.put_i32(
            CEF_WINDOW_INFO_BOUNDS_OFFSET + 12,
            i32::try_from(height).unwrap_or(i32::MAX),
        );
        info.put_i32(CEF_WINDOW_INFO_WINDOWLESS_OFFSET, 1);
        info.put_i32(CEF_WINDOW_INFO_SHARED_TEXTURE_OFFSET, 0);
        info.put_i32(CEF_WINDOW_INFO_EXTERNAL_BEGIN_FRAME_OFFSET, 0);
        info.put_i32(
            CEF_WINDOW_INFO_RUNTIME_STYLE_OFFSET,
            CEF_RUNTIME_STYLE_ALLOY,
        );
        info
    }

    fn as_ptr(&self) -> *const c_void {
        self.bytes.as_ptr().cast()
    }

    fn put_usize(&mut self, offset: usize, value: usize) {
        self.bytes[offset..offset + std::mem::size_of::<usize>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn put_i32(&mut self, offset: usize, value: i32) {
        self.bytes[offset..offset + std::mem::size_of::<i32>()]
            .copy_from_slice(&value.to_ne_bytes());
    }
}

struct CefBrowserSettings {
    bytes: [u8; CEF_BROWSER_SETTINGS_SIZE],
}

impl CefBrowserSettings {
    fn windowless(frame_rate: i32) -> Self {
        let mut settings = Self {
            bytes: [0; CEF_BROWSER_SETTINGS_SIZE],
        };
        settings.put_usize(0, CEF_BROWSER_SETTINGS_SIZE);
        settings.put_i32(CEF_BROWSER_SETTINGS_FRAME_RATE_OFFSET, frame_rate);
        settings.put_u32(CEF_BROWSER_SETTINGS_BACKGROUND_COLOR_OFFSET, 0xFFFF_FFFF);
        settings
    }

    fn as_ptr(&self) -> *const c_void {
        self.bytes.as_ptr().cast()
    }

    fn put_usize(&mut self, offset: usize, value: usize) {
        self.bytes[offset..offset + std::mem::size_of::<usize>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn put_i32(&mut self, offset: usize, value: i32) {
        self.bytes[offset..offset + std::mem::size_of::<i32>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn put_u32(&mut self, offset: usize, value: u32) {
        self.bytes[offset..offset + std::mem::size_of::<u32>()]
            .copy_from_slice(&value.to_ne_bytes());
    }
}

struct CefStringOwned {
    text: String,
    _data: Vec<u16>,
    raw: CefString,
}

impl CefStringOwned {
    fn new(text: &str) -> Result<Self, CefBrowserError> {
        if text.encode_utf16().any(|unit| unit == 0) {
            return Err(CefBrowserError::BadUrl);
        }
        let data = text.encode_utf16().collect::<Vec<_>>();
        let raw = CefString {
            str_: data.as_ptr(),
            length: data.len(),
            dtor: 0,
        };
        Ok(Self {
            text: text.to_owned(),
            _data: data,
            raw,
        })
    }

    fn as_ptr(&self) -> *const c_void {
        (&self.raw as *const CefString).cast()
    }

    fn text(&self) -> &str {
        &self.text
    }
}

#[repr(C)]
struct CefString {
    str_: *const u16,
    length: usize,
    dtor: usize,
}

/// `cef_range_t` — two `uint32_t` (`from`, `to`); `sizeof == 8` confirmed against
/// the pinned CEF 149 `internal/cef_types.h`. Used for IME composition/selection
/// ranges on `cef_browser_host_t::ime_*`.
#[repr(C)]
struct CefRange {
    from: u32,
    to: u32,
}

struct CefBrowserCallbacks {
    state: Box<CefBrowserState>,
    client: Box<CefCallbackBlock<CEF_CLIENT_SIZE>>,
    life_span: Box<CefCallbackBlock<CEF_LIFE_SPAN_HANDLER_SIZE>>,
    render: Box<CefCallbackBlock<CEF_RENDER_HANDLER_SIZE>>,
    request: Box<CefCallbackBlock<CEF_REQUEST_HANDLER_SIZE>>,
    resource_request: Box<CefCallbackBlock<CEF_RESOURCE_REQUEST_HANDLER_SIZE>>,
    print: Box<CefCallbackBlock<CEF_PRINT_HANDLER_SIZE>>,
    display: Box<CefCallbackBlock<CEF_DISPLAY_HANDLER_SIZE>>,
    load: Box<CefCallbackBlock<CEF_LOAD_HANDLER_SIZE>>,
    find: Box<CefCallbackBlock<CEF_FIND_HANDLER_SIZE>>,
    download: Box<CefCallbackBlock<CEF_DOWNLOAD_HANDLER_SIZE>>,
    jsdialog: Box<CefCallbackBlock<CEF_JSDIALOG_HANDLER_SIZE>>,
    audio: Box<CefCallbackBlock<CEF_AUDIO_HANDLER_SIZE>>,
    permission: Box<CefCallbackBlock<CEF_PERMISSION_HANDLER_SIZE>>,
}

impl CefBrowserCallbacks {
    fn new(
        width: u32,
        height: u32,
        stream: Option<&UnixStream>,
        string_userfree_free: CefStringUserfreeUtf16Free,
        string_list_size: CefStringListSize,
        string_list_value: CefStringListValue,
    ) -> Result<Self, CefBrowserError> {
        let state = Box::new(CefBrowserState::new(
            width,
            height,
            stream,
            string_userfree_free,
            string_list_size,
            string_list_value,
        )?);
        let mut callbacks = Self {
            state,
            client: Box::new(CefCallbackBlock::new(CEF_CLIENT_SIZE)),
            life_span: Box::new(CefCallbackBlock::new(CEF_LIFE_SPAN_HANDLER_SIZE)),
            render: Box::new(CefCallbackBlock::new(CEF_RENDER_HANDLER_SIZE)),
            request: Box::new(CefCallbackBlock::new(CEF_REQUEST_HANDLER_SIZE)),
            resource_request: Box::new(CefCallbackBlock::new(CEF_RESOURCE_REQUEST_HANDLER_SIZE)),
            print: Box::new(CefCallbackBlock::new(CEF_PRINT_HANDLER_SIZE)),
            display: Box::new(CefCallbackBlock::new(CEF_DISPLAY_HANDLER_SIZE)),
            load: Box::new(CefCallbackBlock::new(CEF_LOAD_HANDLER_SIZE)),
            find: Box::new(CefCallbackBlock::new(CEF_FIND_HANDLER_SIZE)),
            download: Box::new(CefCallbackBlock::new(CEF_DOWNLOAD_HANDLER_SIZE)),
            jsdialog: Box::new(CefCallbackBlock::new(CEF_JSDIALOG_HANDLER_SIZE)),
            audio: Box::new(CefCallbackBlock::new(CEF_AUDIO_HANDLER_SIZE)),
            permission: Box::new(CefCallbackBlock::new(CEF_PERMISSION_HANDLER_SIZE)),
        };
        callbacks.install();
        Ok(callbacks)
    }

    fn install(&mut self) {
        self.client.put_fn(
            CEF_CLIENT_GET_LIFE_SPAN_HANDLER_OFFSET,
            fn_ptr(get_life_span_handler as *const ()),
        );
        self.client.put_fn(
            CEF_CLIENT_GET_RENDER_HANDLER_OFFSET,
            fn_ptr(get_render_handler as *const ()),
        );
        self.client.put_fn(
            CEF_CLIENT_GET_REQUEST_HANDLER_OFFSET,
            fn_ptr(get_request_handler as *const ()),
        );
        self.client.put_fn(
            CEF_CLIENT_GET_PRINT_HANDLER_OFFSET,
            fn_ptr(get_print_handler as *const ()),
        );
        self.life_span.put_fn(
            CEF_LIFE_SPAN_ON_AFTER_CREATED_OFFSET,
            fn_ptr(on_after_created as *const ()),
        );
        self.life_span.put_fn(
            CEF_LIFE_SPAN_ON_BEFORE_CLOSE_OFFSET,
            fn_ptr(on_before_close as *const ()),
        );
        // window.open / target=_blank → cancel the native popup (windowless CEF
        // cannot host one) and hand the URL to the shell as a new-tab request.
        self.life_span.put_fn(
            CEF_LIFE_SPAN_ON_BEFORE_POPUP_OFFSET,
            fn_ptr(on_before_popup as *const ()),
        );
        self.render.put_fn(
            CEF_RENDER_HANDLER_GET_VIEW_RECT_OFFSET,
            fn_ptr(get_view_rect as *const ()),
        );
        self.render.put_fn(
            CEF_RENDER_HANDLER_ON_PAINT_OFFSET,
            fn_ptr(on_paint as *const ()),
        );
        // The `<select>`/autocomplete popup surface (composited over the view).
        self.render.put_fn(
            CEF_RENDER_HANDLER_ON_POPUP_SHOW_OFFSET,
            fn_ptr(on_popup_show as *const ()),
        );
        self.render.put_fn(
            CEF_RENDER_HANDLER_ON_POPUP_SIZE_OFFSET,
            fn_ptr(on_popup_size as *const ()),
        );
        self.request.put_fn(
            CEF_REQUEST_HANDLER_GET_RESOURCE_REQUEST_HANDLER_OFFSET,
            fn_ptr(get_resource_request_handler as *const ()),
        );
        // Renderer-process death → the shell's sad-tab (EventMsg::Crashed).
        self.request.put_fn(
            CEF_REQUEST_HANDLER_ON_RENDER_PROCESS_TERMINATED_OFFSET,
            fn_ptr(on_render_process_terminated as *const ()),
        );
        // TLS/certificate validation failure → the shell's "Not secure — blocked"
        // interstitial (EventMsg::CertError). We return 0 (blocking-by-default),
        // so CEF cancels the load; no "proceed anyway" this unit.
        self.request.put_fn(
            CEF_REQUEST_HANDLER_ON_CERTIFICATE_ERROR_OFFSET,
            fn_ptr(on_certificate_error as *const ()),
        );
        self.resource_request.put_fn(
            CEF_RESOURCE_REQUEST_HANDLER_ON_BEFORE_RESOURCE_LOAD_OFFSET,
            fn_ptr(on_before_resource_load as *const ()),
        );
        self.print.put_fn(
            CEF_PRINT_HANDLER_ON_PRINT_DIALOG_OFFSET,
            fn_ptr(on_print_dialog as *const ()),
        );
        self.print.put_fn(
            CEF_PRINT_HANDLER_ON_PRINT_JOB_OFFSET,
            fn_ptr(on_print_job as *const ()),
        );
        self.print.put_fn(
            CEF_PRINT_HANDLER_GET_PDF_PAPER_SIZE_OFFSET,
            fn_ptr(get_pdf_paper_size as *const ()),
        );
        // B1 — nav/load/title state feeding the chrome's omnibox + back/forward +
        // loading indicator (the wire + shell already consume EventMsg::NavState/Title).
        self.client.put_fn(
            CEF_CLIENT_GET_DISPLAY_HANDLER_OFFSET,
            fn_ptr(get_display_handler as *const ()),
        );
        self.client.put_fn(
            CEF_CLIENT_GET_LOAD_HANDLER_OFFSET,
            fn_ptr(get_load_handler as *const ()),
        );
        self.display.put_fn(
            CEF_DISPLAY_HANDLER_ON_ADDRESS_CHANGE_OFFSET,
            fn_ptr(on_address_change as *const ()),
        );
        self.display.put_fn(
            CEF_DISPLAY_HANDLER_ON_TITLE_CHANGE_OFFSET,
            fn_ptr(on_title_change as *const ()),
        );
        self.display.put_fn(
            CEF_DISPLAY_HANDLER_ON_CURSOR_CHANGE_OFFSET,
            fn_ptr(on_cursor_change as *const ()),
        );
        // Favicon fetch: the engine reports the page's icon URLs; we pull the PNG
        // through CEF's sandboxed connection and forward the bytes to the shell.
        self.display.put_fn(
            CEF_DISPLAY_HANDLER_ON_FAVICON_URLCHANGE_OFFSET,
            fn_ptr(on_favicon_urlchange as *const ()),
        );
        // HTML5 page fullscreen: the engine reports enter/leave; the shell hides its
        // chrome (edge-to-edge page), matching the F11 immersive mode.
        self.display.put_fn(
            CEF_DISPLAY_HANDLER_ON_FULLSCREEN_MODE_CHANGE_OFFSET,
            fn_ptr(on_fullscreen_mode_change as *const ()),
        );
        self.load.put_fn(
            CEF_LOAD_HANDLER_ON_LOADING_STATE_CHANGE_OFFSET,
            fn_ptr(on_loading_state_change as *const ()),
        );
        // Find-in-page match tally (native find via the browser host).
        self.client.put_fn(
            CEF_CLIENT_GET_FIND_HANDLER_OFFSET,
            fn_ptr(get_find_handler as *const ()),
        );
        self.find.put_fn(
            CEF_FIND_HANDLER_ON_FIND_RESULT_OFFSET,
            fn_ptr(on_find_result as *const ()),
        );
        // Download interception → the mesh Transfers ledger (B2): cancel CEF's own
        // write, forward the URL so the shell submits a daemon transfer.
        self.client.put_fn(
            CEF_CLIENT_GET_DOWNLOAD_HANDLER_OFFSET,
            fn_ptr(get_download_handler as *const ()),
        );
        self.download.put_fn(
            CEF_DOWNLOAD_HANDLER_CAN_DOWNLOAD_OFFSET,
            fn_ptr(can_download as *const ()),
        );
        self.download.put_fn(
            CEF_DOWNLOAD_HANDLER_ON_BEFORE_DOWNLOAD_OFFSET,
            fn_ptr(on_before_download as *const ()),
        );
        // JS dialogs (alert/confirm/prompt): emit a non-blocking notice to the
        // shell and auto-resolve the dialog so the page never hangs. Kept off the
        // size-keyed `lookup_peer` (its sizeof 72 collides with the load handler)
        // by carrying a dedicated `jsdialog_handler_ptr`, mirroring the print path.
        self.client.put_fn(
            CEF_CLIENT_GET_JSDIALOG_HANDLER_OFFSET,
            fn_ptr(get_jsdialog_handler as *const ()),
        );
        self.jsdialog.put_fn(
            CEF_JSDIALOG_HANDLER_ON_JSDIALOG_OFFSET,
            fn_ptr(on_jsdialog as *const ()),
        );
        self.jsdialog.put_fn(
            CEF_JSDIALOG_HANDLER_ON_BEFORE_UNLOAD_DIALOG_OFFSET,
            fn_ptr(on_before_unload_dialog as *const ()),
        );
        // Per-page audible state → the shell's 🔊 tab indicator. get_audio_parameters
        // MUST return non-zero (with a sane STEREO/48kHz default) or CEF never spins
        // up a stream and the started/stopped callbacks stay silent. Carried on a
        // dedicated `audio_handler_ptr` (never the size-keyed `lookup_peer`).
        self.client.put_fn(
            CEF_CLIENT_GET_AUDIO_HANDLER_OFFSET,
            fn_ptr(get_audio_handler as *const ()),
        );
        self.audio.put_fn(
            CEF_AUDIO_HANDLER_GET_AUDIO_PARAMETERS_OFFSET,
            fn_ptr(get_audio_parameters as *const ()),
        );
        self.audio.put_fn(
            CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STARTED_OFFSET,
            fn_ptr(on_audio_stream_started as *const ()),
        );
        self.audio.put_fn(
            CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_PACKET_OFFSET,
            fn_ptr(on_audio_stream_packet as *const ()),
        );
        self.audio.put_fn(
            CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_STOPPED_OFFSET,
            fn_ptr(on_audio_stream_stopped as *const ()),
        );
        self.audio.put_fn(
            CEF_AUDIO_HANDLER_ON_AUDIO_STREAM_ERROR_OFFSET,
            fn_ptr(on_audio_stream_error as *const ()),
        );
        // Per-site permission grants (geolocation / notifications / clipboard /
        // camera / microphone): CEF asks via on_show_permission_prompt or
        // on_request_media_access_permission; we round-trip to the shell and grant
        // or deny on its answer (session-only). Carried on a dedicated
        // `permission_handler_ptr` (never the size-keyed `lookup_peer`), mirroring
        // the audio/jsdialog path.
        self.client.put_fn(
            CEF_CLIENT_GET_PERMISSION_HANDLER_OFFSET,
            fn_ptr(get_permission_handler as *const ()),
        );
        self.permission.put_fn(
            CEF_PERMISSION_HANDLER_ON_REQUEST_MEDIA_ACCESS_OFFSET,
            fn_ptr(on_request_media_access_permission as *const ()),
        );
        self.permission.put_fn(
            CEF_PERMISSION_HANDLER_ON_SHOW_PROMPT_OFFSET,
            fn_ptr(on_show_permission_prompt as *const ()),
        );
        self.permission.put_fn(
            CEF_PERMISSION_HANDLER_ON_DISMISS_PROMPT_OFFSET,
            fn_ptr(on_dismiss_permission_prompt as *const ()),
        );

        let state = self.state.as_ref() as *const CefBrowserState as usize;
        self.state
            .print_handler_ptr
            .store(self.print.as_usize(), Ordering::SeqCst);
        self.state
            .display_handler_ptr
            .store(self.display.as_usize(), Ordering::SeqCst);
        self.state
            .load_handler_ptr
            .store(self.load.as_usize(), Ordering::SeqCst);
        self.state
            .find_handler_ptr
            .store(self.find.as_usize(), Ordering::SeqCst);
        self.state
            .download_handler_ptr
            .store(self.download.as_usize(), Ordering::SeqCst);
        self.state
            .jsdialog_handler_ptr
            .store(self.jsdialog.as_usize(), Ordering::SeqCst);
        self.state
            .audio_handler_ptr
            .store(self.audio.as_usize(), Ordering::SeqCst);
        self.state
            .permission_handler_ptr
            .store(self.permission.as_usize(), Ordering::SeqCst);
        let mut registry = registry().lock().expect("cef callback registry");
        registry.insert(self.client.as_usize(), state);
        registry.insert(self.life_span.as_usize(), state);
        registry.insert(self.render.as_usize(), state);
        registry.insert(self.request.as_usize(), state);
        registry.insert(self.resource_request.as_usize(), state);
        registry.insert(self.print.as_usize(), state);
        registry.insert(self.display.as_usize(), state);
        registry.insert(self.load.as_usize(), state);
        registry.insert(self.find.as_usize(), state);
        registry.insert(self.download.as_usize(), state);
        registry.insert(self.jsdialog.as_usize(), state);
        registry.insert(self.audio.as_usize(), state);
        registry.insert(self.permission.as_usize(), state);
    }

    fn client_ptr(&self) -> *mut c_void {
        self.client.as_mut_ptr()
    }

    fn created(&self) -> usize {
        self.state.created.load(Ordering::SeqCst)
    }

    fn closed(&self) -> usize {
        self.state.closed.load(Ordering::SeqCst)
    }

    fn paints(&self) -> usize {
        self.state.paints.load(Ordering::SeqCst)
    }

    fn navigations(&self) -> u64 {
        self.state.navigations()
    }

    fn loading(&self) -> bool {
        self.state.nav_loading.load(Ordering::SeqCst)
    }

    fn last_paint_width(&self) -> i32 {
        self.state.last_paint_width.load(Ordering::SeqCst) as i32
    }

    fn last_paint_height(&self) -> i32 {
        self.state.last_paint_height.load(Ordering::SeqCst) as i32
    }

    fn resize(&self, width: u32, height: u32) {
        self.state.resize(width, height);
    }

    fn update_pointer(&self, x: f32, y: f32) -> (i32, i32) {
        self.state.update_pointer(x, y)
    }

    fn pointer_position(&self) -> (i32, i32) {
        self.state.pointer_position()
    }

    fn register_press(&self, button: PointerButton, x: i32, y: i32) -> i32 {
        self.state.register_press(button, x, y)
    }

    fn register_release(&self, button: PointerButton) -> i32 {
        self.state.register_release(button)
    }

    fn held_button_flags(&self) -> c_int {
        self.state.held_button_flags()
    }

    fn apply_resource_verdict(&self, id: u64, allow: bool) {
        self.state.apply_resource_verdict(id, allow);
    }

    fn apply_permission_decision(&self, id: u64, allow: bool) {
        self.state.apply_permission_decision(id, allow);
    }

    fn apply_before_unload_decision(&self, id: u64, proceed: bool) {
        self.state.apply_before_unload_decision(id, proceed);
    }

    fn retain_pdf_callback(&self) -> *mut c_void {
        self.state.retain_pdf_callback()
    }
}

impl Drop for CefBrowserCallbacks {
    fn drop(&mut self) {
        self.state.cancel_pending_resource_requests();
        self.state.release_pending_permission_prompts();
        self.state.release_pending_before_unload_dialogs();
        let mut registry = registry().lock().expect("cef callback registry");
        registry.remove(&self.client.as_usize());
        registry.remove(&self.life_span.as_usize());
        registry.remove(&self.render.as_usize());
        registry.remove(&self.request.as_usize());
        registry.remove(&self.resource_request.as_usize());
        registry.remove(&self.print.as_usize());
        registry.remove(&self.display.as_usize());
        registry.remove(&self.load.as_usize());
        registry.remove(&self.find.as_usize());
        registry.remove(&self.download.as_usize());
        registry.remove(&self.audio.as_usize());
        registry.remove(&self.permission.as_usize());
        self.state.purge_finished_pdf_callbacks(None);
        if let Ok(callbacks) = self.state.pdf_callbacks.lock() {
            for callback in callbacks.iter() {
                registry.remove(&callback.as_usize());
            }
        }
        self.state.purge_finished_download_image_callbacks(None);
        if let Ok(callbacks) = self.state.download_image_callbacks.lock() {
            for callback in callbacks.iter() {
                registry.remove(&callback.as_usize());
            }
        }
        self.state.purge_finished_page_text_visitors(None);
        if let Ok(callbacks) = self.state.page_text_visitors.lock() {
            for callback in callbacks.iter() {
                registry.remove(&callback.as_usize());
            }
        }
    }
}

/// Double/triple-click chaining window (Chromium's default double-click time).
const CLICK_COUNT_WINDOW: Duration = Duration::from_millis(500);
/// Max pointer travel per axis (device px) between presses that still chains a
/// multi-click — a slightly sloppy double-click still counts.
const CLICK_COUNT_RADIUS_PX: i32 = 5;
/// Web multi-click semantics top out at triple (paragraph select); further rapid
/// clicks hold at 3 rather than cycling back to a caret.
const CLICK_COUNT_MAX: i32 = 3;

/// Chromium-style multi-click detection. The egui shell forwards plain
/// press/release events with no count, so the bridge derives the `click_count`
/// CEF expects (dblclick → word select → paragraph select) exactly like a native
/// Chromium window: consecutive same-button presses within
/// [`CLICK_COUNT_WINDOW`] and [`CLICK_COUNT_RADIUS_PX`] chain the count.
struct ClickTracker {
    last: Option<ClickRecord>,
}

struct ClickRecord {
    at: Instant,
    x: i32,
    y: i32,
    button: u8,
    count: i32,
}

impl ClickTracker {
    const fn new() -> Self {
        Self { last: None }
    }

    /// Register a press at `now`/`(x, y)` and return its click count.
    fn register(&mut self, now: Instant, x: i32, y: i32, button: PointerButton) -> i32 {
        let button = button as u8;
        let count = match &self.last {
            Some(last)
                if last.button == button
                    && now.saturating_duration_since(last.at) <= CLICK_COUNT_WINDOW
                    && (last.x - x).abs() <= CLICK_COUNT_RADIUS_PX
                    && (last.y - y).abs() <= CLICK_COUNT_RADIUS_PX =>
            {
                (last.count + 1).min(CLICK_COUNT_MAX)
            }
            _ => 1,
        };
        self.last = Some(ClickRecord {
            at: now,
            x,
            y,
            button,
            count,
        });
        count
    }

    /// The count of the most recent press of `button` — its release must carry
    /// the same count — or 1 if none is tracked.
    fn release_count(&self, button: PointerButton) -> i32 {
        match &self.last {
            Some(last) if last.button == button as u8 => last.count,
            _ => 1,
        }
    }
}

/// The engine's popup widget overlay (a `<select>` dropdown or autocomplete
/// list). A windowless CEF browser paints the popup as a SEPARATE `PET_POPUP`
/// surface — never into the view frame — so unless the bridge composites it,
/// dropdowns are invisible. While visible, the latest clean view frame is
/// retained so a popup repaint (or hide) can republish without a fresh view paint.
#[derive(Default)]
struct PopupOverlay {
    visible: bool,
    /// Popup rect in view device px: `(x, y, width, height)` from `on_popup_size`.
    rect: (i32, i32, i32, i32),
    /// Latest `PET_POPUP` BGRA paint (rect-sized).
    pixels: Option<Vec<u8>>,
    /// Retained clean view frame `(width, height, bgra)` while the popup shows.
    view: Option<(u32, u32, Vec<u8>)>,
}

impl PopupOverlay {
    /// The retained view frame with the popup blended in, if both are present.
    fn compose(&self) -> Option<(u32, u32, Vec<u8>)> {
        let (view_w, view_h, view) = self.view.as_ref()?;
        let pixels = self.pixels.as_ref()?;
        let (x, y, w, h) = self.rect;
        let mut merged = view.clone();
        blend_popup_over_view(&mut merged, *view_w, *view_h, pixels, w, h, x, y);
        Some((*view_w, *view_h, merged))
    }
}

/// Copy an opaque BGRA popup rect over a BGRA view frame at `(at_x, at_y)` view
/// coordinates, clipping to the view bounds (a dropdown near the window edge
/// may extend past it). Row-wise `copy_from_slice`; both buffers are tightly
/// packed `width * height * 4`.
fn blend_popup_over_view(
    view: &mut [u8],
    view_w: u32,
    view_h: u32,
    popup: &[u8],
    popup_w: i32,
    popup_h: i32,
    at_x: i32,
    at_y: i32,
) {
    let (view_w, view_h) = (i64::from(view_w), i64::from(view_h));
    let (popup_w, popup_h) = (i64::from(popup_w), i64::from(popup_h));
    if popup_w <= 0 || popup_h <= 0 {
        return;
    }
    for row in 0..popup_h {
        let view_y = i64::from(at_y) + row;
        if view_y < 0 || view_y >= view_h {
            continue;
        }
        let src_col = (-i64::from(at_x)).max(0);
        let dst_col = i64::from(at_x).max(0);
        let cols = (popup_w - src_col).min(view_w - dst_col);
        if cols <= 0 {
            continue;
        }
        let src = usize::try_from((row * popup_w + src_col) * 4).unwrap_or(usize::MAX);
        let dst = usize::try_from((view_y * view_w + dst_col) * 4).unwrap_or(usize::MAX);
        let len = usize::try_from(cols * 4).unwrap_or(0);
        if let (Some(src_end), Some(dst_end)) = (src.checked_add(len), dst.checked_add(len)) {
            if src_end <= popup.len() && dst_end <= view.len() {
                view[dst..dst_end].copy_from_slice(&popup[src..src_end]);
            }
        }
    }
}

struct CefBrowserState {
    width: AtomicI32,
    height: AtomicI32,
    created: AtomicUsize,
    closed: AtomicUsize,
    paints: AtomicUsize,
    /// Monotonic count of main/sub-frame navigations, bumped from the resource
    /// handler's `is_navigation` flag (browser-8). Drives per-context shim
    /// re-injection without a wall-clock timer.
    nav_seq: AtomicU64,
    last_paint_width: AtomicUsize,
    last_paint_height: AtomicUsize,
    pointer_x: AtomicI32,
    pointer_y: AtomicI32,
    frame_sink: Mutex<Option<BrowserFrameSink>>,
    next_resource_request_id: AtomicU64,
    /// In-flight subresource verdict callbacks. Bounded by
    /// [`MAX_PENDING_RESOURCE_REQUESTS`] so a wedged shell or hostile page cannot
    /// grow CEF callback retention without limit.
    pending_resource_requests: Mutex<HashMap<u64, usize>>,
    /// One-shot `print_to_pdf` callbacks retained until CEF reports completion.
    /// Finished callbacks are purged on the next PDF lifecycle touch so repeated
    /// Save PDF operations do not retain callback boxes indefinitely.
    pdf_callbacks: Mutex<Vec<Box<CefCallbackBlock<CEF_PDF_PRINT_CALLBACK_SIZE>>>>,
    /// Finished PDF callback pointers waiting for a safe purge pass. We avoid
    /// dropping the callback object from inside its own C callback.
    finished_pdf_callbacks: Mutex<HashSet<usize>>,
    /// One-shot favicon `download_image` callbacks retained until CEF's async
    /// delivery returns. Completed callbacks are purged on the next favicon
    /// lifecycle touch so churn stays bounded by in-flight callbacks plus the
    /// just-returned callback object.
    download_image_callbacks: Mutex<Vec<Box<CefCallbackBlock<CEF_DOWNLOAD_IMAGE_CALLBACK_SIZE>>>>,
    /// Finished favicon callback pointers waiting for a safe purge pass. We avoid
    /// dropping the callback object from inside its own C callback.
    finished_download_image_callbacks: Mutex<HashSet<usize>>,
    /// Native visible-text visitors retained until CEF replies to `get_text`.
    /// CEF page-text is used by spellcheck/TTS/translate/offline-cache and must
    /// not depend on page JavaScript or synthetic resource loads.
    page_text_visitors: Mutex<Vec<Box<PageTextVisitor>>>,
    /// Finished page-text visitor pointers waiting for a safe purge pass. We avoid
    /// dropping the visitor object from inside its own C callback.
    finished_page_text_visitors: Mutex<HashSet<usize>>,
    print_handler_ptr: AtomicUsize,
    /// Cached child-handler block pointers, set at `install()` time — resolved
    /// DIRECTLY (not via the size-keyed `lookup_peer`, whose `callback_size`
    /// whitelist omits these sizes and whose find(48) aliases pdf_print(48)).
    /// Returning null here silently disables the handler on live CEF.
    display_handler_ptr: AtomicUsize,
    load_handler_ptr: AtomicUsize,
    find_handler_ptr: AtomicUsize,
    download_handler_ptr: AtomicUsize,
    /// The jsdialog handler block address (alert/confirm/prompt). Stored directly
    /// like `print_handler_ptr` because its sizeof (72) collides with the load
    /// handler under the size-keyed `lookup_peer`.
    jsdialog_handler_ptr: AtomicUsize,
    /// The audio handler block address (per-page audible state → the 🔊 tab
    /// indicator). Stored directly like the other child handlers so the callback
    /// resolves without the size-keyed `lookup_peer`.
    audio_handler_ptr: AtomicUsize,
    /// The permission handler block address (per-site geolocation / notifications /
    /// clipboard / camera / microphone grants). Cached directly like the other
    /// child handlers, never the size-keyed `lookup_peer`.
    permission_handler_ptr: AtomicUsize,
    /// In-flight permission callbacks, keyed by CEF's `prompt_id` or a
    /// bridge-minted media-access id. Bounded by [`MAX_PENDING_PERMISSION_PROMPTS`].
    /// Each callback is a live refcounted CEF object: add_ref'd when stashed,
    /// continued on the shell's `ControlMsg::PermissionDecision`, then released.
    pending_permission_prompts: Mutex<HashMap<u64, PendingPermissionCallback>>,
    /// Monotonic id minted for CEF media-access prompts. The media-access callback
    /// has no prompt id, so the bridge creates one for the wire round-trip.
    next_media_permission_id: AtomicU64,
    /// Monotonic id minted for CEF beforeunload prompts. CEF's callback does not
    /// include a prompt id, so the bridge creates one for the wire round-trip.
    next_before_unload_id: AtomicU64,
    /// In-flight beforeunload callbacks, keyed by the bridge-minted id. Bounded by
    /// [`MAX_PENDING_BEFORE_UNLOAD_DIALOGS`]. Each is a live refcounted
    /// `cef_jsdialog_callback_t*`: add_ref'd when stashed in
    /// `on_before_unload_dialog`, released after `cont()` on the shell's
    /// `ControlMsg::BeforeUnloadDecision` or teardown.
    pending_before_unload_dialogs: Mutex<HashMap<u64, usize>>,
    string_userfree_free: CefStringUserfreeUtf16Free,
    /// `cef_string_list_size` / `cef_string_list_value` exports (dlsym'd via the
    /// ABI), used to read the favicon `icon_urls` list in `on_favicon_urlchange`.
    string_list_size: CefStringListSize,
    string_list_value: CefStringListValue,
    /// B1 nav state, assembled across the display + load handlers: the committed
    /// URL (from `on_address_change`) and the loading/history flags (from
    /// `on_loading_state_change`), published together as `EventMsg::NavState`.
    nav_url: Mutex<String>,
    nav_loading: AtomicBool,
    nav_can_back: AtomicBool,
    nav_can_forward: AtomicBool,
    /// Currently-held pointer buttons as CEF `EVENTFLAG_*_MOUSE_BUTTON` bits —
    /// ORed into mouse-move events so a press-drag reaches the page as a drag
    /// (text selection, scrollbar-thumb drag) instead of a hover.
    held_buttons: AtomicI32,
    /// Multi-click chaining state (double/triple-click detection).
    click_tracker: Mutex<ClickTracker>,
    /// The `<select>`/autocomplete popup overlay composited over the view frame.
    popup: Mutex<PopupOverlay>,
    /// Last cursor kind sent (as its wire byte), so identical repeats — CEF fires
    /// on_cursor_change on every mouse-move — are not re-published each frame.
    last_cursor: AtomicI32,
    /// Monotonic id minted per intercepted download (B2), keying the shell row.
    download_seq: AtomicU64,
    /// The shell-supplied real HTTP `User-Agent` override (empty = none). Set from
    /// `ControlMsg::SetUserAgent` alongside the `navigator.userAgent` JS shim, and
    /// stamped onto every outgoing request's `User-Agent:` header in
    /// `on_before_resource_load` so server-side sniffers see the spoofed agent too
    /// (the JS shim only fools client-side `navigator.userAgent` reads).
    user_agent_override: Mutex<String>,
    /// Per-tab autoplay policy remembered across document contexts. The active
    /// document is patched immediately, and the navigation shim injector reapplies
    /// the block to fresh documents while this is true.
    autoplay_blocked: AtomicBool,
    /// CEF audio callback says the page is currently audible.
    audio_active: AtomicBool,
    /// Media Session / HTMLMediaElement probe says at least one media element is
    /// playing. This is metadata only; no samples or frames leave CEF.
    media_session_playing: AtomicBool,
    /// Optional legacy WebRTC API remover. Default is false so CEF can satisfy
    /// browser-page WebRTC compatibility; deployments can set
    /// [`CEF_WEBRTC_BLOCK_ENV`] to restore the old WebRTC-off posture.
    webrtc_blocked: AtomicBool,
}

impl CefBrowserState {
    fn new(
        width: u32,
        height: u32,
        stream: Option<&UnixStream>,
        string_userfree_free: CefStringUserfreeUtf16Free,
        string_list_size: CefStringListSize,
        string_list_value: CefStringListValue,
    ) -> Result<Self, CefBrowserError> {
        let frame_sink = stream
            .map(|stream| {
                let helper_stream = stream.try_clone()?;
                let sink = OffscreenFrameSink::attach(&helper_stream, width, height)?;
                Ok::<_, CefBrowserError>(BrowserFrameSink {
                    stream: helper_stream,
                    sink,
                })
            })
            .transpose()?;
        Ok(Self {
            width: AtomicI32::new(i32::try_from(width).unwrap_or(i32::MAX)),
            height: AtomicI32::new(i32::try_from(height).unwrap_or(i32::MAX)),
            created: AtomicUsize::new(0),
            closed: AtomicUsize::new(0),
            paints: AtomicUsize::new(0),
            nav_seq: AtomicU64::new(0),
            last_paint_width: AtomicUsize::new(0),
            last_paint_height: AtomicUsize::new(0),
            pointer_x: AtomicI32::new(0),
            pointer_y: AtomicI32::new(0),
            frame_sink: Mutex::new(frame_sink),
            next_resource_request_id: AtomicU64::new(1),
            pending_resource_requests: Mutex::new(HashMap::new()),
            pdf_callbacks: Mutex::new(Vec::new()),
            finished_pdf_callbacks: Mutex::new(HashSet::new()),
            download_image_callbacks: Mutex::new(Vec::new()),
            finished_download_image_callbacks: Mutex::new(HashSet::new()),
            page_text_visitors: Mutex::new(Vec::new()),
            finished_page_text_visitors: Mutex::new(HashSet::new()),
            print_handler_ptr: AtomicUsize::new(0),
            display_handler_ptr: AtomicUsize::new(0),
            load_handler_ptr: AtomicUsize::new(0),
            find_handler_ptr: AtomicUsize::new(0),
            download_handler_ptr: AtomicUsize::new(0),
            jsdialog_handler_ptr: AtomicUsize::new(0),
            audio_handler_ptr: AtomicUsize::new(0),
            permission_handler_ptr: AtomicUsize::new(0),
            pending_permission_prompts: Mutex::new(HashMap::new()),
            next_media_permission_id: AtomicU64::new(MEDIA_PERMISSION_ID_BASE),
            next_before_unload_id: AtomicU64::new(1),
            pending_before_unload_dialogs: Mutex::new(HashMap::new()),
            string_userfree_free,
            string_list_size,
            string_list_value,
            nav_url: Mutex::new(String::new()),
            nav_loading: AtomicBool::new(false),
            nav_can_back: AtomicBool::new(false),
            nav_can_forward: AtomicBool::new(false),
            held_buttons: AtomicI32::new(0),
            click_tracker: Mutex::new(ClickTracker::new()),
            popup: Mutex::new(PopupOverlay::default()),
            last_cursor: AtomicI32::new(-1),
            download_seq: AtomicU64::new(1),
            user_agent_override: Mutex::new(String::new()),
            autoplay_blocked: AtomicBool::new(false),
            audio_active: AtomicBool::new(false),
            media_session_playing: AtomicBool::new(false),
            webrtc_blocked: AtomicBool::new(cef_webrtc_blocked_from_env()),
        })
    }

    /// Store (or clear, when empty) the shell's real HTTP `User-Agent` override.
    /// The next `on_before_resource_load` stamps it onto the request header.
    fn set_user_agent_override(&self, user_agent: &str) {
        if let Ok(mut guard) = self.user_agent_override.lock() {
            *guard = user_agent.to_owned();
        }
    }

    /// The current `User-Agent` override, or `None` when unset/empty.
    fn user_agent_override(&self) -> Option<String> {
        self.user_agent_override
            .lock()
            .ok()
            .map(|guard| guard.clone())
            .filter(|ua| !ua.is_empty())
    }

    /// B2 — a download was intercepted; forward its URL + name so the shell
    /// submits a daemon Transfers job (CEF's own write is cancelled by the caller).
    fn publish_download_intercepted(&self, url: String, filename: String) {
        let id = self.download_seq.fetch_add(1, Ordering::SeqCst);
        let event = EventMsg::Download {
            id,
            url,
            filename,
            received: 0,
            total: 0,
            done: false,
            canceled: false,
        };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// Publish the engine's cursor shape to the shell, coalescing repeats (CEF
    /// re-reports the cursor on every mouse-move).
    fn publish_cursor(&self, kind: CursorKind) {
        let byte = i32::from(kind as u8);
        if self.last_cursor.swap(byte, Ordering::SeqCst) == byte {
            return;
        }
        let event = EventMsg::CursorChanged { kind };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// Forward an HTML5 fullscreen enter/leave to the shell (it hides/shows chrome).
    fn publish_fullscreen(&self, enabled: bool) {
        let event = EventMsg::Fullscreen { enabled };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// Forward the page's audible state (audio stream started/stopped) to the
    /// shell, which shows/hides the 🔊 "playing audio" indicator on the tab. We
    /// carry only the audible bit — never any audio samples.
    fn publish_audio_state(&self, audible: bool) {
        self.audio_active.store(audible, Ordering::SeqCst);
        let event = EventMsg::AudioState { audible };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// Forward bounded page/media-session now-playing metadata to the shell.
    fn publish_media_metadata(&self, body: String) {
        self.media_session_playing
            .store(media_metadata_reports_playing(&body), Ordering::SeqCst);
        let event = EventMsg::MediaMetadata {
            body: clamp_utf8(&body, CEF_MEDIA_METADATA_BEACON_MAX_BYTES),
        };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// Forward an engine-decoded favicon (PNG bytes) to the shell, which uploads
    /// it as the tab-strip icon.
    fn publish_favicon(&self, png: Vec<u8>) {
        let event = EventMsg::Favicon { png };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// Publish a BGRA frame to the shell over the shm frame channel.
    fn publish_frame(&self, width: u32, height: u32, pixels: &[u8]) {
        if let Ok(mut guard) = self.frame_sink.lock() {
            if let Some(frame_sink) = guard.as_mut() {
                let _ = frame_sink
                    .sink
                    .publish_bgra(&frame_sink.stream, width, height, pixels);
            }
        }
    }

    /// Record a button press: mark it held (for drag mouse-moves) and chain the
    /// multi-click count this press reports to CEF.
    fn register_press(&self, button: PointerButton, x: i32, y: i32) -> i32 {
        self.held_buttons
            .fetch_or(mouse_button_event_flag(button), Ordering::SeqCst);
        self.click_tracker
            .lock()
            .map(|mut tracker| tracker.register(Instant::now(), x, y, button))
            .unwrap_or(1)
    }

    /// Record a button release; the release event carries its press's count.
    fn register_release(&self, button: PointerButton) -> i32 {
        self.held_buttons
            .fetch_and(!mouse_button_event_flag(button), Ordering::SeqCst);
        self.click_tracker
            .lock()
            .map(|tracker| tracker.release_count(button))
            .unwrap_or(1)
    }

    /// The currently-held pointer buttons as CEF event flags.
    fn held_button_flags(&self) -> c_int {
        self.held_buttons.load(Ordering::SeqCst)
    }

    fn resize(&self, width: u32, height: u32) {
        self.width
            .store(i32::try_from(width).unwrap_or(i32::MAX), Ordering::SeqCst);
        self.height
            .store(i32::try_from(height).unwrap_or(i32::MAX), Ordering::SeqCst);
    }

    /// Record that a fresh document context is being loaded (browser-8). Called
    /// from the resource handler when CEF flags a request as a navigation.
    fn record_navigation(&self) {
        self.audio_active.store(false, Ordering::SeqCst);
        self.media_session_playing.store(false, Ordering::SeqCst);
        self.nav_seq.fetch_add(1, Ordering::SeqCst);
    }

    /// Current navigation generation observed by the pump loop.
    fn navigations(&self) -> u64 {
        self.nav_seq.load(Ordering::SeqCst)
    }

    fn active_media(&self) -> bool {
        self.audio_active.load(Ordering::SeqCst)
            || self.media_session_playing.load(Ordering::SeqCst)
    }

    fn update_pointer(&self, x: f32, y: f32) -> (i32, i32) {
        let x = f32_to_i32(x);
        let y = f32_to_i32(y);
        self.pointer_x.store(x, Ordering::SeqCst);
        self.pointer_y.store(y, Ordering::SeqCst);
        (x, y)
    }

    fn pointer_position(&self) -> (i32, i32) {
        (
            self.pointer_x.load(Ordering::SeqCst),
            self.pointer_y.load(Ordering::SeqCst),
        )
    }

    fn current_top_level_url(&self) -> String {
        self.nav_url.lock().map(|u| u.clone()).unwrap_or_default()
    }

    fn login_beacon_matches_top_level(&self, origin: &str) -> bool {
        hosts_match(origin, &self.current_top_level_url())
    }

    fn host_matches_top_level(&self, expected_host: &str) -> bool {
        let Some(expected) = credential_host(expected_host) else {
            return false;
        };
        let Some(current) = credential_host(&self.current_top_level_url()) else {
            return false;
        };
        expected == current
    }

    fn begin_resource_request(&self, url: String, callback: *mut c_void) -> c_int {
        if let Some((id, text)) = decode_page_text_beacon(&url) {
            self.publish_page_text(id, text);
            if !callback.is_null() {
                cancel_cef_callback(callback);
            }
            return RV_CANCEL;
        }
        if let Some((id, body)) = decode_page_scrape_beacon(&url) {
            self.publish_page_scrape(id, body);
            if !callback.is_null() {
                cancel_cef_callback(callback);
            }
            return RV_CANCEL;
        }
        if let Some(body) = decode_passkey_beacon(&url) {
            self.publish_passkey_request(body);
            if !callback.is_null() {
                cancel_cef_callback(callback);
            }
            return RV_CANCEL;
        }
        if let Some(body) = decode_media_metadata_beacon(&url) {
            self.publish_media_metadata(body);
            if !callback.is_null() {
                cancel_cef_callback(callback);
            }
            return RV_CANCEL;
        }
        if url.starts_with(CEF_LOGIN_BEACON_PREFIX) {
            if let Some((origin, body)) = decode_login_beacon(&url) {
                if self.login_beacon_matches_top_level(&origin) {
                    self.publish_login_submitted(origin, body);
                }
            }
            if !callback.is_null() {
                cancel_cef_callback(callback);
            }
            return RV_CANCEL;
        }
        if callback.is_null() {
            return RV_CONTINUE;
        }
        let Some(id) = self
            .next_resource_request_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .ok()
        else {
            return RV_CONTINUE;
        };
        let Ok(mut pending) = self.pending_resource_requests.lock() else {
            cancel_cef_callback(callback);
            return RV_CANCEL;
        };
        if pending.len() >= MAX_PENDING_RESOURCE_REQUESTS {
            drop(pending);
            cancel_cef_callback(callback);
            return RV_CANCEL;
        }
        add_ref_cef(callback);
        pending.insert(id, callback as usize);
        drop(pending);

        let event = EventMsg::ResourceRequest {
            id,
            url,
            resource: RESOURCE_OTHER,
        };
        let sent = self
            .frame_sink
            .lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().and_then(|frame_sink| {
                    sock::send_frame(&frame_sink.stream, &event.encode()).ok()
                })
            })
            .is_some();
        if sent {
            return RV_CONTINUE_ASYNC;
        }
        if let Ok(mut pending) = self.pending_resource_requests.lock() {
            pending.remove(&id);
        }
        cancel_cef_callback(callback);
        release_cef(callback);
        RV_CANCEL
    }

    fn apply_resource_verdict(&self, id: u64, allow: bool) {
        let callback = self
            .pending_resource_requests
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id))
            .map(|ptr| ptr as *mut c_void);
        if let Some(callback) = callback {
            if allow {
                continue_cef_callback(callback);
            } else {
                cancel_cef_callback(callback);
            }
            release_cef(callback);
        }
    }

    fn retain_pdf_callback(&self) -> *mut c_void {
        self.purge_finished_pdf_callbacks(None);
        let mut callback = Box::new(CefCallbackBlock::new(CEF_PDF_PRINT_CALLBACK_SIZE));
        callback.put_fn(
            CEF_PDF_PRINT_CALLBACK_ON_FINISHED_OFFSET,
            fn_ptr(on_pdf_print_finished as *const ()),
        );
        let ptr = callback.as_mut_ptr();
        if let Ok(mut callbacks) = self.pdf_callbacks.lock() {
            let state = self as *const CefBrowserState as usize;
            if let Ok(mut registry) = registry().lock() {
                registry.insert(callback.as_usize(), state);
            }
            callbacks.push(callback);
            ptr
        } else {
            ptr::null_mut()
        }
    }

    fn finish_pdf_callback(&self, callback: *mut c_void) {
        let key = callback as usize;
        if key == 0 {
            return;
        }
        self.purge_finished_pdf_callbacks(Some(key));
        if let Ok(mut registry) = registry().lock() {
            registry.remove(&key);
        }
        if let Ok(mut finished) = self.finished_pdf_callbacks.lock() {
            finished.insert(key);
        }
    }

    fn purge_finished_pdf_callbacks(&self, keep: Option<usize>) {
        let finished = self
            .finished_pdf_callbacks
            .lock()
            .ok()
            .map(|mut finished| {
                let ready: Vec<usize> = finished
                    .iter()
                    .copied()
                    .filter(|key| Some(*key) != keep)
                    .collect();
                for key in &ready {
                    finished.remove(key);
                }
                ready
            })
            .unwrap_or_default();
        if finished.is_empty() {
            return;
        }
        if let Ok(mut callbacks) = self.pdf_callbacks.lock() {
            callbacks.retain(|callback| !finished.contains(&callback.as_usize()));
        }
    }

    #[cfg(test)]
    fn retained_pdf_callback_count(&self) -> usize {
        self.pdf_callbacks
            .lock()
            .map(|callbacks| callbacks.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn finished_pdf_callback_count(&self) -> usize {
        self.finished_pdf_callbacks
            .lock()
            .map(|callbacks| callbacks.len())
            .unwrap_or(0)
    }

    /// Allocate a one-shot `cef_download_image_callback_t` for a favicon fetch,
    /// wired to [`on_download_image_finished`]. The Box is retained in
    /// `download_image_callbacks` for CEF's async delivery and registered so
    /// `with_state` resolves it in the callback.
    fn retain_download_image_callback(&self) -> *mut c_void {
        self.purge_finished_download_image_callbacks(None);
        let mut callback = Box::new(CefCallbackBlock::new(CEF_DOWNLOAD_IMAGE_CALLBACK_SIZE));
        callback.put_fn(
            CEF_DOWNLOAD_IMAGE_CALLBACK_ON_FINISHED_OFFSET,
            fn_ptr(on_download_image_finished as *const ()),
        );
        let ptr = callback.as_mut_ptr();
        if let Ok(mut callbacks) = self.download_image_callbacks.lock() {
            let state = self as *const CefBrowserState as usize;
            if let Ok(mut registry) = registry().lock() {
                registry.insert(callback.as_usize(), state);
            }
            callbacks.push(callback);
            ptr
        } else {
            ptr::null_mut()
        }
    }

    fn finish_download_image_callback(&self, callback: *mut c_void) {
        let key = callback as usize;
        if key == 0 {
            return;
        }
        self.purge_finished_download_image_callbacks(Some(key));
        if let Ok(mut registry) = registry().lock() {
            registry.remove(&key);
        }
        if let Ok(mut finished) = self.finished_download_image_callbacks.lock() {
            finished.insert(key);
        }
    }

    fn purge_finished_download_image_callbacks(&self, keep: Option<usize>) {
        let finished = self
            .finished_download_image_callbacks
            .lock()
            .ok()
            .map(|mut finished| {
                let ready: Vec<usize> = finished
                    .iter()
                    .copied()
                    .filter(|key| Some(*key) != keep)
                    .collect();
                for key in &ready {
                    finished.remove(key);
                }
                ready
            })
            .unwrap_or_default();
        if finished.is_empty() {
            return;
        }
        if let Ok(mut callbacks) = self.download_image_callbacks.lock() {
            callbacks.retain(|callback| !finished.contains(&callback.as_usize()));
        }
    }

    #[cfg(test)]
    fn retained_download_image_callback_count(&self) -> usize {
        self.download_image_callbacks
            .lock()
            .map(|callbacks| callbacks.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn finished_download_image_callback_count(&self) -> usize {
        self.finished_download_image_callbacks
            .lock()
            .map(|callbacks| callbacks.len())
            .unwrap_or(0)
    }

    /// Allocate a one-shot `cef_string_visitor_t` for native visible page text.
    /// The visitor is retained until CEF invokes `visit`; overflow declines the
    /// native request and lets the JavaScript beacon fallback try instead.
    fn retain_page_text_visitor(&self, id: u64, max_bytes: u32) -> *mut c_void {
        self.purge_finished_page_text_visitors(None);
        let Ok(mut visitors) = self.page_text_visitors.lock() else {
            return ptr::null_mut();
        };
        if visitors.len() >= MAX_PENDING_PAGE_TEXT_VISITORS {
            return ptr::null_mut();
        }
        let visitor = Box::new(PageTextVisitor::new(
            id,
            max_bytes.clamp(1, CEF_PAGE_TEXT_BEACON_MAX_BYTES),
        ));
        let ptr = visitor.as_mut_ptr();
        let state = self as *const CefBrowserState as usize;
        if let Ok(mut registry) = registry().lock() {
            registry.insert(visitor.as_usize(), state);
        }
        visitors.push(visitor);
        ptr
    }

    fn finish_page_text_visitor(&self, visitor: *mut c_void) -> Option<(u64, u32)> {
        let key = visitor as usize;
        if key == 0 {
            return None;
        }
        let metadata = self.page_text_visitors.lock().ok().and_then(|visitors| {
            visitors
                .iter()
                .find(|visitor| visitor.as_usize() == key)
                .map(|visitor| (visitor.id, visitor.max_bytes))
        });
        self.purge_finished_page_text_visitors(Some(key));
        if let Ok(mut registry) = registry().lock() {
            registry.remove(&key);
        }
        if let Ok(mut finished) = self.finished_page_text_visitors.lock() {
            finished.insert(key);
        }
        metadata
    }

    fn purge_finished_page_text_visitors(&self, keep: Option<usize>) {
        let finished = self
            .finished_page_text_visitors
            .lock()
            .ok()
            .map(|mut finished| {
                let ready: Vec<usize> = finished
                    .iter()
                    .copied()
                    .filter(|key| Some(*key) != keep)
                    .collect();
                for key in &ready {
                    finished.remove(key);
                }
                ready
            })
            .unwrap_or_default();
        if finished.is_empty() {
            return;
        }
        if let Ok(mut visitors) = self.page_text_visitors.lock() {
            visitors.retain(|visitor| !finished.contains(&visitor.as_usize()));
        }
    }

    #[cfg(test)]
    fn retained_page_text_visitor_count(&self) -> usize {
        self.page_text_visitors
            .lock()
            .map(|visitors| visitors.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn finished_page_text_visitor_count(&self) -> usize {
        self.finished_page_text_visitors
            .lock()
            .map(|visitors| visitors.len())
            .unwrap_or(0)
    }

    /// The engine reported the page's favicon URLs. Read the first, then pull the
    /// image through CEF's sandboxed connection via `download_image` — the decoded
    /// PNG arrives asynchronously in [`on_download_image_finished`].
    fn request_favicon(&self, browser: *mut c_void, icon_urls: *mut c_void) {
        if icon_urls.is_null() {
            return;
        }
        // SAFETY: `icon_urls` is the live `cef_string_list_t` CEF passed to the
        // display handler; `string_list_size` is its matching libcef export.
        let count = unsafe { (self.string_list_size)(icon_urls) };
        if count == 0 {
            return;
        }
        let mut first = CefString {
            str_: ptr::null(),
            length: 0,
            dtor: 0,
        };
        // SAFETY: `cef_string_list_value` copies element 0 into `first`, allocating
        // a heap buffer whose `dtor` we invoke below. The out-pointer is a live,
        // zeroed `cef_string_t`.
        let got = unsafe {
            (self.string_list_value)(icon_urls, 0, (&mut first as *mut CefString).cast())
        };
        if got == 0 || first.str_.is_null() || first.length == 0 {
            free_cef_string_copy(&mut first);
            return;
        }
        let Some(host) = browser_host(browser) else {
            free_cef_string_copy(&mut first);
            return;
        };
        let Some(download_image) = read_fn(host, CEF_BROWSER_HOST_DOWNLOAD_IMAGE_OFFSET) else {
            free_cef_string_copy(&mut first);
            return;
        };
        let callback = self.retain_download_image_callback();
        if callback.is_null() {
            free_cef_string_copy(&mut first);
            return;
        }
        // SAFETY: `download_image` is read from `cef_browser_host_t::download_image`,
        // whose pinned C signature is `(cef_browser_host_t*, const cef_string_t*,
        // int is_favicon, uint32_t max_image_size, int bypass_cache,
        // cef_download_image_callback_t*)`.
        let download_image: unsafe extern "C" fn(
            *mut c_void,
            *const CefString,
            c_int,
            u32,
            c_int,
            *mut c_void,
        ) = unsafe { std::mem::transmute(download_image) };
        // SAFETY: `host` came from CEF, `first` is a live `cef_string_t` for the
        // call (CEF copies the URL synchronously), and `callback` is retained by
        // the browser state until shutdown.
        unsafe { download_image(host, &first as *const CefString, 1, 32, 0, callback) };
        free_cef_string_copy(&mut first);
    }

    fn publish_pdf_finished(&self, path: String, ok: bool) {
        let ok = ok && pdf_file_looks_written(&path);
        let event = EventMsg::PdfSaved { path, ok };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    fn publish_page_text(&self, id: u64, text: String) {
        let event = EventMsg::PageText { id, text };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    fn publish_page_scrape(&self, id: u64, body: String) {
        let event = EventMsg::PageScrape { id, body };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    fn publish_passkey_request(&self, body: String) {
        let event = EventMsg::PasskeyRequest { body };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    fn publish_login_submitted(&self, origin: String, body: String) {
        let event = EventMsg::LoginSubmitted { origin, body };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// B1 — emit the current nav snapshot (URL + loading/history flags) so the
    /// shell chrome lights up the omnibox, Back/Forward, and the loading control.
    fn publish_nav_state(&self) {
        let url = self.nav_url.lock().map(|u| u.clone()).unwrap_or_default();
        let event = EventMsg::NavState {
            can_back: self.nav_can_back.load(Ordering::SeqCst),
            can_forward: self.nav_can_forward.load(Ordering::SeqCst),
            loading: self.nav_loading.load(Ordering::SeqCst),
            url,
        };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// B1 — emit the page title so the chrome labels the tab with it (not the URL).
    fn publish_title(&self, title: String) {
        let event = EventMsg::Title(title);
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// The page asked for a new window/tab — forward the URL so the shell opens
    /// it as a regular tab (the native popup is cancelled by the caller).
    fn publish_popup_requested(&self, url: String) {
        let event = EventMsg::PopupRequested { url };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// A native find-in-page result — the shell shows the "active/count" tally.
    fn publish_find_result(&self, count: u32, active: u32, final_update: bool) {
        let event = EventMsg::FindResult {
            count,
            active,
            final_update,
        };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// The renderer process died — tell the shell so the tab shows its crashed
    /// (sad-tab) state instead of a frozen last frame.
    fn publish_crashed(&self, reason: String) {
        let event = EventMsg::Crashed { reason };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// A TLS/certificate error blocked the top-level load — tell the shell so it
    /// paints the "Not secure — blocked" interstitial over the dead frame.
    fn publish_cert_error(&self, url: String, code: i32, message: &str) {
        let event = EventMsg::CertError {
            url,
            code,
            message: message.to_owned(),
        };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// A page opened a JS dialog (alert/confirm/prompt). Forward a non-blocking
    /// notice to the shell (which may later surface a passive notice); the engine
    /// itself auto-resolves the dialog so the page never hangs.
    fn publish_js_dialog(&self, kind: u8, message: String, origin: String) {
        let event = EventMsg::JsDialog {
            kind,
            message,
            origin,
        };
        let _ = self.frame_sink.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|frame_sink| sock::send_frame(&frame_sink.stream, &event.encode()).ok())
        });
    }

    /// CEF asks whether a page's `beforeunload` handler should proceed. Unlike
    /// alert/confirm/prompt, this is a real blocking navigation decision: retain
    /// CEF's JS-dialog callback, publish a shell prompt, and continue only after
    /// `ControlMsg::BeforeUnloadDecision`. If emission/stashing fails, cancel the
    /// unload synchronously (`success=0`) so the page stays put and no callback
    /// leaks.
    fn begin_before_unload_dialog(
        &self,
        message: String,
        is_reload: bool,
        callback: *mut c_void,
    ) -> c_int {
        if callback.is_null() {
            return 0;
        }
        let Some(id) = self
            .next_before_unload_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .ok()
        else {
            continue_jsdialog_callback(callback, 0);
            return 1;
        };
        let Ok(mut pending) = self.pending_before_unload_dialogs.lock() else {
            continue_jsdialog_callback(callback, 0);
            return 1;
        };
        if pending.len() >= MAX_PENDING_BEFORE_UNLOAD_DIALOGS {
            drop(pending);
            continue_jsdialog_callback(callback, 0);
            return 1;
        }
        add_ref_cef(callback);
        pending.insert(id, callback as usize);
        drop(pending);

        let event = EventMsg::BeforeUnloadDialog {
            id,
            message,
            origin: self.current_top_level_url(),
            is_reload,
        };
        let sent = self
            .frame_sink
            .lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().and_then(|frame_sink| {
                    sock::send_frame(&frame_sink.stream, &event.encode()).ok()
                })
            })
            .is_some();
        if sent {
            return 1;
        }
        if let Ok(mut pending) = self.pending_before_unload_dialogs.lock() {
            pending.remove(&id);
        }
        continue_jsdialog_callback(callback, 0);
        release_cef(callback);
        1
    }

    /// The shell answered a beforeunload prompt. Remove the stashed callback before
    /// calling CEF so re-entrant callbacks cannot double-release the object.
    fn apply_before_unload_decision(&self, id: u64, proceed: bool) {
        let callback = self
            .pending_before_unload_dialogs
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id))
            .map(|ptr| ptr as *mut c_void);
        if let Some(callback) = callback {
            continue_jsdialog_callback(callback, c_int::from(proceed));
            release_cef(callback);
        }
    }

    fn cancel_pending_resource_requests(&self) {
        let callbacks = self
            .pending_resource_requests
            .lock()
            .map(|mut pending| pending.drain().map(|(_, ptr)| ptr).collect::<Vec<_>>())
            .unwrap_or_default();
        for callback in callbacks {
            let callback = callback as *mut c_void;
            cancel_cef_callback(callback);
            release_cef(callback);
        }
    }

    #[cfg(test)]
    fn pending_resource_request_count(&self) -> usize {
        self.pending_resource_requests
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }

    fn begin_permission_prompt_for_kind(
        &self,
        prompt_id: u64,
        origin: String,
        kind: u8,
        pending_callback: PendingPermissionCallback,
    ) -> c_int {
        let callback = pending_callback.callback_ptr();
        if callback.is_null() {
            return 0;
        }
        let Ok(mut pending) = self.pending_permission_prompts.lock() else {
            pending_callback.deny();
            return 1;
        };
        if pending.len() >= MAX_PENDING_PERMISSION_PROMPTS || pending.contains_key(&prompt_id) {
            drop(pending);
            pending_callback.deny();
            return 1;
        }
        // The callback is a live refcounted CEF object; retain it across the async
        // shell round-trip. Paired with the `release_cef` in
        // `apply_permission_decision` / `discard_permission_prompt` / teardown.
        add_ref_cef(callback);
        pending.insert(prompt_id, pending_callback);
        drop(pending);

        let event = EventMsg::PermissionRequest {
            id: prompt_id,
            kind,
            origin,
        };
        let sent = self
            .frame_sink
            .lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().and_then(|frame_sink| {
                    sock::send_frame(&frame_sink.stream, &event.encode()).ok()
                })
            })
            .is_some();
        if sent {
            return 1;
        }
        if let Ok(mut pending) = self.pending_permission_prompts.lock() {
            pending.remove(&prompt_id);
        }
        // Could not emit or stash: undo the ref and let CEF apply default handling.
        release_cef(callback);
        0
    }

    /// CEF asks us to show a generic permission prompt (`on_show_permission_prompt`).
    /// Map the requested types to our wire `kind`; out-of-scope requests return 0
    /// so CEF applies default handling (Alloy-style deny) — no prompt, no callback
    /// retention. For an in-scope kind we add_ref the callback, emit
    /// `EventMsg::PermissionRequest`, stash the callback under `prompt_id`, and
    /// return 1 (we continue it on the shell's `PermissionDecision`).
    fn begin_permission_prompt(
        &self,
        prompt_id: u64,
        origin: String,
        requested_permissions: u32,
        callback: *mut c_void,
    ) -> c_int {
        let Some(kind) = permission_kind_from_cef(requested_permissions) else {
            return 0;
        };
        self.begin_permission_prompt_for_kind(
            prompt_id,
            origin,
            kind,
            PendingPermissionCallback::Prompt {
                callback: callback as usize,
            },
        )
    }

    /// CEF asks for camera/microphone media access (`getUserMedia`). This callback
    /// is separate from generic permission prompts: allowing requires echoing the
    /// requested media-access bitmask back to CEF, and denying is `Continue(0)`.
    fn begin_media_access_permission(
        &self,
        origin: String,
        requested_permissions: u32,
        callback: *mut c_void,
    ) -> c_int {
        let Some(kind) = media_access_kind_from_cef(requested_permissions) else {
            return 0;
        };
        let prompt_id = self
            .next_media_permission_id
            .fetch_add(1, Ordering::Relaxed);
        self.begin_permission_prompt_for_kind(
            prompt_id,
            origin,
            kind,
            PendingPermissionCallback::MediaAccess {
                callback: callback as usize,
                requested_permissions,
            },
        )
    }

    /// The shell answered a permission prompt (`ControlMsg::PermissionDecision`).
    /// Look up the stashed callback by `id`, continue it with ACCEPT/DENY, then
    /// release our stash ref and drop the entry. A missing id is a safe no-op (the
    /// prompt was already answered, dismissed, or never in scope). The map lock is
    /// released before `cont()` so a synchronous CEF `on_dismiss` re-entry cannot
    /// deadlock or double-release.
    fn apply_permission_decision(&self, id: u64, allow: bool) {
        let pending_callback = self
            .pending_permission_prompts
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        if let Some(pending_callback) = pending_callback {
            pending_callback.answer(allow);
            release_cef(pending_callback.callback_ptr());
        }
    }

    /// CEF dismissed a prompt itself (navigation / closure) via
    /// `on_dismiss_permission_prompt`. Drop our stash ref WITHOUT calling `cont`
    /// (CEF owns the dismissal). A missing id — already answered by the shell — is a
    /// safe no-op; the map removal guarantees we never release twice.
    fn discard_permission_prompt(&self, id: u64) {
        let pending_callback = self
            .pending_permission_prompts
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        if let Some(pending_callback) = pending_callback {
            release_cef(pending_callback.callback_ptr());
        }
    }

    /// Drain any permission-prompt callbacks still stashed at browser teardown: deny
    /// each (satisfying CEF's "Continue must be called" contract for a handled
    /// prompt) and drop our stash ref. Mirrors `cancel_pending_resource_requests`.
    fn release_pending_permission_prompts(&self) {
        let callbacks = self
            .pending_permission_prompts
            .lock()
            .map(|mut pending| pending.drain().map(|(_, ptr)| ptr).collect::<Vec<_>>())
            .unwrap_or_default();
        for pending_callback in callbacks {
            pending_callback.deny();
            release_cef(pending_callback.callback_ptr());
        }
    }

    #[cfg(test)]
    fn pending_permission_prompt_count(&self) -> usize {
        self.pending_permission_prompts
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }

    /// Drain unresolved beforeunload callbacks at teardown. `success=0` is the
    /// conservative answer (stay/cancel), and it satisfies CEF's handled-callback
    /// contract before dropping our ref.
    fn release_pending_before_unload_dialogs(&self) {
        let callbacks = self
            .pending_before_unload_dialogs
            .lock()
            .map(|mut pending| pending.drain().map(|(_, ptr)| ptr).collect::<Vec<_>>())
            .unwrap_or_default();
        for callback in callbacks {
            let callback = callback as *mut c_void;
            continue_jsdialog_callback(callback, 0);
            release_cef(callback);
        }
    }

    #[cfg(test)]
    fn pending_before_unload_count(&self) -> usize {
        self.pending_before_unload_dialogs
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }
}

struct CefMouseEvent {
    bytes: [u8; CEF_MOUSE_EVENT_SIZE],
}

impl CefMouseEvent {
    fn new(x: i32, y: i32, modifiers: c_int) -> Self {
        let mut event = Self {
            bytes: [0; CEF_MOUSE_EVENT_SIZE],
        };
        event.put_i32(CEF_MOUSE_EVENT_X_OFFSET, x);
        event.put_i32(CEF_MOUSE_EVENT_Y_OFFSET, y);
        event.put_i32(CEF_MOUSE_EVENT_MODIFIERS_OFFSET, modifiers);
        event
    }

    fn as_ptr(&self) -> *const c_void {
        self.bytes.as_ptr().cast()
    }

    fn put_i32(&mut self, offset: usize, value: i32) {
        self.bytes[offset..offset + std::mem::size_of::<i32>()]
            .copy_from_slice(&value.to_ne_bytes());
    }
}

struct CefKeyEvent {
    bytes: [u8; CEF_KEY_EVENT_SIZE],
}

impl CefKeyEvent {
    fn new(
        event_type: c_int,
        modifiers: c_int,
        windows_key_code: i32,
        native_key_code: i32,
        character: u16,
        unmodified_character: u16,
    ) -> Self {
        let mut event = Self {
            bytes: [0; CEF_KEY_EVENT_SIZE],
        };
        event.put_usize(0, CEF_KEY_EVENT_SIZE);
        event.put_i32(CEF_KEY_EVENT_TYPE_OFFSET, event_type);
        event.put_i32(CEF_KEY_EVENT_MODIFIERS_OFFSET, modifiers);
        event.put_i32(CEF_KEY_EVENT_WINDOWS_KEY_CODE_OFFSET, windows_key_code);
        event.put_i32(CEF_KEY_EVENT_NATIVE_KEY_CODE_OFFSET, native_key_code);
        event.put_i32(CEF_KEY_EVENT_IS_SYSTEM_KEY_OFFSET, 0);
        event.put_u16(CEF_KEY_EVENT_CHARACTER_OFFSET, character);
        event.put_u16(
            CEF_KEY_EVENT_UNMODIFIED_CHARACTER_OFFSET,
            unmodified_character,
        );
        // Offscreen CEF does not get native toolkit focus metadata from a window.
        // The shell sends keyboard/text only after the page canvas owns focus, so
        // mark the event as editable-focused for Chromium's text-input path.
        event.put_i32(CEF_KEY_EVENT_FOCUS_ON_EDITABLE_FIELD_OFFSET, 1);
        event
    }

    fn as_ptr(&self) -> *const c_void {
        self.bytes.as_ptr().cast()
    }

    fn put_i32(&mut self, offset: usize, value: i32) {
        self.bytes[offset..offset + std::mem::size_of::<i32>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn put_usize(&mut self, offset: usize, value: usize) {
        self.bytes[offset..offset + std::mem::size_of::<usize>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn put_u16(&mut self, offset: usize, value: u16) {
        self.bytes[offset..offset + std::mem::size_of::<u16>()]
            .copy_from_slice(&value.to_ne_bytes());
    }
}

#[derive(Clone, Copy, Debug)]
enum PendingPermissionCallback {
    Prompt {
        callback: usize,
    },
    MediaAccess {
        callback: usize,
        requested_permissions: u32,
    },
}

impl PendingPermissionCallback {
    fn callback_ptr(self) -> *mut c_void {
        match self {
            Self::Prompt { callback } | Self::MediaAccess { callback, .. } => {
                callback as *mut c_void
            }
        }
    }

    fn answer(self, allow: bool) {
        match self {
            Self::Prompt { callback } => {
                let result = if allow {
                    CEF_PERMISSION_RESULT_ACCEPT
                } else {
                    CEF_PERMISSION_RESULT_DENY
                };
                continue_permission_callback(callback as *mut c_void, result);
            }
            Self::MediaAccess {
                callback,
                requested_permissions,
            } => {
                let allowed_permissions = if allow { requested_permissions } else { 0 };
                continue_media_access_callback(callback as *mut c_void, allowed_permissions);
            }
        }
    }

    fn deny(self) {
        self.answer(false);
    }
}

struct BrowserFrameSink {
    stream: UnixStream,
    sink: OffscreenFrameSink,
}

#[repr(C, align(8))]
struct CefCallbackBlock<const N: usize> {
    bytes: [u8; N],
}

impl<const N: usize> CefCallbackBlock<N> {
    fn new(size: usize) -> Self {
        let mut block = Self { bytes: [0; N] };
        block.put_usize(BASE_SIZE_OFFSET, size);
        block.put_fn(BASE_ADD_REF_OFFSET, fn_ptr(add_ref as *const ()));
        block.put_fn(BASE_RELEASE_OFFSET, fn_ptr(release as *const ()));
        block.put_fn(BASE_HAS_ONE_REF_OFFSET, fn_ptr(has_one_ref as *const ()));
        block.put_fn(
            BASE_HAS_AT_LEAST_ONE_REF_OFFSET,
            fn_ptr(has_at_least_one_ref as *const ()),
        );
        block
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.bytes.as_ptr().cast_mut().cast()
    }

    fn as_usize(&self) -> usize {
        self.as_mut_ptr() as usize
    }

    fn put_usize(&mut self, offset: usize, value: usize) {
        self.bytes[offset..offset + std::mem::size_of::<usize>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn put_fn(&mut self, offset: usize, value: usize) {
        self.put_usize(offset, value);
    }
}

fn fn_ptr(ptr: *const ()) -> usize {
    ptr as usize
}

struct PageTextVisitor {
    block: CefCallbackBlock<CEF_STRING_VISITOR_SIZE>,
    id: u64,
    max_bytes: u32,
}

impl PageTextVisitor {
    fn new(id: u64, max_bytes: u32) -> Self {
        let mut block = CefCallbackBlock::new(CEF_STRING_VISITOR_SIZE);
        block.put_fn(
            CEF_STRING_VISITOR_VISIT_OFFSET,
            fn_ptr(on_page_text_visited as *const ()),
        );
        Self {
            block,
            id,
            max_bytes,
        }
    }

    fn as_mut_ptr(&self) -> *mut c_void {
        self.block.as_mut_ptr()
    }

    fn as_usize(&self) -> usize {
        self.block.as_usize()
    }
}

#[repr(C)]
struct CefRect {
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CefSize {
    width: c_int,
    height: c_int,
}

/// `cef_audio_parameters_t` — the POD struct CEF passes (by out-pointer to
/// `get_audio_parameters`, by const-pointer to `on_audio_stream_started`). Leads
/// with a `size_t size` then three 4-byte ints; matches `CEF_AUDIO_PARAMETERS_SIZE`
/// (24). The `size` field is what pins `channel_layout` to offset 8 — omitting it
/// aliased `channel_layout` onto `size` and dropped `sample_rate`/`frames_per_buffer`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CefAudioParameters {
    /// `size_t size` — CEF pre-fills this with `sizeof(cef_audio_parameters_t)`.
    size: usize,
    /// A `cef_channel_layout_t` enum value (see `CEF_CHANNEL_LAYOUT_STEREO`).
    channel_layout: c_int,
    sample_rate: c_int,
    frames_per_buffer: c_int,
}

unsafe extern "C" fn add_ref(_self: *mut c_void) {}

unsafe extern "C" fn release(_self: *mut c_void) -> c_int {
    0
}

unsafe extern "C" fn has_one_ref(_self: *mut c_void) -> c_int {
    0
}

unsafe extern "C" fn has_at_least_one_ref(_self: *mut c_void) -> c_int {
    1
}

unsafe extern "C" fn get_life_span_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.life_span_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_render_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.render_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_request_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.request_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_print_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.print_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_resource_request_handler(
    self_: *mut c_void,
    browser: *mut c_void,
    frame: *mut c_void,
    _request: *mut c_void,
    is_navigation: c_int,
    _is_download: c_int,
    _request_initiator: *const c_void,
    disable_default_handling: *mut c_int,
) -> *mut c_void {
    if !disable_default_handling.is_null() {
        // SAFETY: CEF supplied this out-parameter for the callback duration.
        unsafe {
            *disable_default_handling = 0;
        }
    }
    with_state(self_, |state| {
        if is_navigation != 0 && frame_is_main(browser, frame) {
            // browser-8: a real navigation opens a fresh JS context that needs
            // the WebRTC/passkey shims re-injected. Signal the pump loop instead
            // of re-running the shims on a blind 250 ms timer.
            state.record_navigation();
        }
        state.resource_request_ptr()
    })
    .unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn on_after_created(self_: *mut c_void, _browser: *mut c_void) {
    let _ = with_state(self_, |state| {
        state.created.fetch_add(1, Ordering::SeqCst);
    });
}

unsafe extern "C" fn on_before_close(self_: *mut c_void, _browser: *mut c_void) {
    let _ = with_state(self_, |state| {
        state.closed.fetch_add(1, Ordering::SeqCst);
    });
}

/// CEF `on_before_popup(...)` — the page asked for a new window (window.open,
/// target=_blank). A windowless offscreen browser cannot host a native popup, so
/// cancel it (return 1) and forward the target URL for the shell to open as a
/// regular tab. Signature pinned to CEF 149's 14-arg layout.
#[allow(clippy::too_many_arguments, reason = "the CEF C vtable signature")]
unsafe extern "C" fn on_before_popup(
    self_: *mut c_void,
    _browser: *mut c_void,
    _frame: *mut c_void,
    _popup_id: c_int,
    target_url: *const CefString,
    _target_frame_name: *const CefString,
    _target_disposition: c_int,
    _user_gesture: c_int,
    _popup_features: *const c_void,
    _window_info: *mut c_void,
    _client: *mut *mut c_void,
    _settings: *mut c_void,
    _extra_info: *mut *mut c_void,
    _no_javascript_access: *mut c_int,
) -> c_int {
    if !target_url.is_null() {
        let url = cef_string_to_string(target_url);
        if !url.is_empty() {
            let _ = with_state(self_, |state| state.publish_popup_requested(url.clone()));
        }
    }
    // 1 = cancel the native popup; the shell opens the URL as a tab instead.
    1
}

unsafe extern "C" fn get_view_rect(self_: *mut c_void, _browser: *mut c_void, rect: *mut CefRect) {
    if rect.is_null() {
        return;
    }
    let _ = with_state(self_, |state| {
        // SAFETY: CEF supplied a non-null output pointer for this callback.
        unsafe {
            (*rect).x = 0;
            (*rect).y = 0;
            (*rect).width = state.width.load(Ordering::SeqCst);
            (*rect).height = state.height.load(Ordering::SeqCst);
        }
    });
}

unsafe extern "C" fn on_paint(
    self_: *mut c_void,
    _browser: *mut c_void,
    paint_type: c_int,
    _dirty_rects_count: usize,
    _dirty_rects: *const CefRect,
    buffer: *const c_void,
    width: c_int,
    height: c_int,
) {
    if buffer.is_null() || width <= 0 || height <= 0 {
        return;
    }
    let len = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    // SAFETY: CEF documents `buffer` as `width * height * 4` bytes of BGRA for
    // both `PET_VIEW` and `PET_POPUP` paints, and the pointer was checked non-null.
    let pixels = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), len) };
    if paint_type == PET_POPUP {
        // A `<select>`/autocomplete popup painted: stash its pixels and, when a
        // clean view frame is retained, republish the composite so the dropdown
        // is visible without waiting for the next view paint.
        let _ = with_state(self_, |state| {
            let merged = state
                .popup
                .lock()
                .ok()
                .map(|mut popup| {
                    // Trust the paint's own dimensions over the last on_popup_size
                    // rect (CEF may repaint after a resize); keep the rect origin.
                    popup.rect.2 = width;
                    popup.rect.3 = height;
                    popup.pixels = Some(pixels.to_vec());
                    popup.compose()
                })
                .unwrap_or(None);
            if let Some((view_w, view_h, frame)) = merged {
                state.publish_frame(view_w, view_h, &frame);
            }
        });
        return;
    }
    if paint_type != PET_VIEW {
        return;
    }
    let _ = with_state(self_, |state| {
        state.paints.fetch_add(1, Ordering::SeqCst);
        state
            .last_paint_width
            .store(usize::try_from(width).unwrap_or(0), Ordering::SeqCst);
        state
            .last_paint_height
            .store(usize::try_from(height).unwrap_or(0), Ordering::SeqCst);
        let width = u32::try_from(width).unwrap_or(0);
        let height = u32::try_from(height).unwrap_or(0);
        // While a popup is visible, retain the clean view frame (so popup
        // repaints/hide can republish) and publish the composited frame instead.
        let composite = state.popup.lock().ok().and_then(|mut popup| {
            if popup.visible {
                popup.view = Some((width, height, pixels.to_vec()));
                popup.compose()
            } else {
                None
            }
        });
        match composite {
            Some((view_w, view_h, frame)) => state.publish_frame(view_w, view_h, &frame),
            None => state.publish_frame(width, height, pixels),
        }
    });
}

/// CEF `on_popup_show(self, browser, show)` — the popup widget toggled. On hide,
/// republish the retained clean view frame so the dropdown pixels are erased.
unsafe extern "C" fn on_popup_show(self_: *mut c_void, _browser: *mut c_void, show: c_int) {
    let _ = with_state(self_, |state| {
        let Ok(mut popup) = state.popup.lock() else {
            return;
        };
        if show == 0 {
            popup.visible = false;
            popup.pixels = None;
            let view = popup.view.take();
            drop(popup);
            if let Some((width, height, frame)) = view {
                state.publish_frame(width, height, &frame);
            }
        } else {
            popup.visible = true;
        }
    });
}

/// CEF `on_popup_size(self, browser, rect)` — where the popup sits in view
/// coordinates (and its size, which the next `PET_POPUP` paint confirms).
unsafe extern "C" fn on_popup_size(
    self_: *mut c_void,
    _browser: *mut c_void,
    rect: *const CefRect,
) {
    if rect.is_null() {
        return;
    }
    // SAFETY: CEF supplies a valid rect for the duration of the callback.
    let (x, y, width, height) = unsafe { ((*rect).x, (*rect).y, (*rect).width, (*rect).height) };
    let _ = with_state(self_, |state| {
        if let Ok(mut popup) = state.popup.lock() {
            popup.rect = (x, y, width, height);
        }
    });
}

/// CEF `on_render_process_terminated(self, browser, status, error_code,
/// error_string)` — the tab's renderer process died. Forward a human-readable
/// reason so the shell swaps the frozen frame for its crashed (sad-tab) state.
unsafe extern "C" fn on_render_process_terminated(
    self_: *mut c_void,
    _browser: *mut c_void,
    status: c_int,
    error_code: c_int,
    error_string: *const CefString,
) {
    // cef_termination_status_t (CEF 149).
    let what = match status {
        1 => "renderer was killed",
        2 => "renderer crashed",
        3 => "renderer ran out of memory",
        4 => "renderer failed to launch",
        5 => "renderer integrity failure",
        _ => "renderer terminated abnormally",
    };
    let detail = if error_string.is_null() {
        String::new()
    } else {
        cef_string_to_string(error_string)
    };
    let reason = if detail.is_empty() {
        format!("{what} (code {error_code})")
    } else {
        format!("{what} (code {error_code}): {detail}")
    };
    let _ = with_state(self_, |state| state.publish_crashed(reason.clone()));
}

/// Map a Chromium `net::Error` cert code (`cef_errorcode_t`) onto a short
/// human message for the shell's "Not secure — blocked" interstitial.
fn cert_error_message(code: i32) -> &'static str {
    match code {
        -200 => "The certificate does not match the site's name",
        -201 => "The certificate is expired or not yet valid",
        -202 => "The certificate is not trusted (unknown authority)",
        -203 => "The certificate has no revocation mechanism",
        -204 => "The certificate's revocation status could not be checked",
        -205 => "The certificate has been revoked",
        -206 => "The certificate is invalid",
        -207 => "The certificate uses a weak signature algorithm",
        -208 => "The certificate's name is non-unique",
        -210 => "The certificate uses a weak key",
        -211 => "The certificate violates a name constraint",
        -212 => "The certificate's validity period is too long",
        -213 => "The certificate is a distrusted Symantec legacy certificate",
        _ => "Certificate error",
    }
}

/// CEF `on_certificate_error(self, browser, cert_error, request_url, ssl_info,
/// callback)` — TLS validation failed on the top-level load. We publish the
/// error and return 0 (secure default): CEF cancels the load, the shell shows a
/// blocking interstitial. The `callback` is intentionally dropped (no "proceed
/// anyway" this unit) and `ssl_info` is left untouched.
unsafe extern "C" fn on_certificate_error(
    self_: *mut c_void,
    _browser: *mut c_void,
    cert_error: c_int,
    request_url: *const CefString,
    _ssl_info: *const c_void,
    _callback: *const c_void,
) -> c_int {
    let url = cef_string_to_string(request_url);
    // `c_int` is `i32` on the pinned Linux target; the wire carries an `i32`.
    let code: i32 = cert_error;
    let message = cert_error_message(code);
    let _ = with_state(self_, |state| state.publish_cert_error(url, code, message));
    // Return 0: do not proceed — CEF cancels the load (blocking-by-default).
    0
}

unsafe extern "C" fn on_before_resource_load(
    self_: *mut c_void,
    _browser: *mut c_void,
    _frame: *mut c_void,
    request: *mut c_void,
    callback: *mut c_void,
) -> c_int {
    with_state(self_, |state| {
        // Override the REAL HTTP `User-Agent:` header (not just the JS-injected
        // `navigator.userAgent`) when the shell supplied one, so server-side
        // sniffers match the spoofed agent. Stamped before the URL extraction so
        // it applies to every outgoing request the resource handler sees.
        if let Some(user_agent) = state.user_agent_override() {
            set_request_header(request, "User-Agent", &user_agent);
        }
        let Some(url) = request_url(request, state.string_userfree_free) else {
            return RV_CONTINUE;
        };
        state.begin_resource_request(url, callback)
    })
    .unwrap_or(RV_CONTINUE)
}

unsafe extern "C" fn get_display_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.display_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_load_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.load_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_find_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.find_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_download_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.download_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_jsdialog_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.jsdialog_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_audio_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.audio_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn get_permission_handler(self_: *mut c_void) -> *mut c_void {
    with_state(self_, |state| state.permission_ptr()).unwrap_or(ptr::null_mut())
}

/// Map a `cef_permission_request_types_t` bitmask (`requested_permissions`) onto
/// our engine-neutral wire `kind`. Geolocation / notifications / clipboard and
/// camera / microphone are in scope (operator-authorized, session-only); every
/// other request (MIDI, sensors, protected-media, …) yields `None` → default deny
/// without a prompt. When several in-scope bits are set we surface the first in
/// wire order (geolocation < notifications < clipboard < camera/microphone).
fn permission_kind_from_cef(requested: u32) -> Option<u8> {
    if requested & CEF_PERMISSION_TYPE_GEOLOCATION != 0 {
        Some(PERMISSION_KIND_GEOLOCATION)
    } else if requested & CEF_PERMISSION_TYPE_NOTIFICATIONS != 0 {
        Some(PERMISSION_KIND_NOTIFICATIONS)
    } else if requested & CEF_PERMISSION_TYPE_CLIPBOARD != 0 {
        Some(PERMISSION_KIND_CLIPBOARD)
    } else {
        let camera = requested & CEF_PERMISSION_TYPE_CAMERA_STREAM != 0;
        let microphone = requested & CEF_PERMISSION_TYPE_MIC_STREAM != 0;
        match (camera, microphone) {
            (true, true) => Some(PERMISSION_KIND_CAMERA_MICROPHONE),
            (true, false) => Some(PERMISSION_KIND_CAMERA),
            (false, true) => Some(PERMISSION_KIND_MICROPHONE),
            (false, false) => None,
        }
    }
}

/// Map CEF's media-access bitmask onto the same shell permission kinds. This path
/// only handles device camera/microphone capture; desktop capture remains out of
/// scope and returns 0 from the CEF callback for default deny.
fn media_access_kind_from_cef(requested: u32) -> Option<u8> {
    const SUPPORTED_CAPTURE: u32 =
        CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE | CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE;
    if requested & !SUPPORTED_CAPTURE != 0 {
        return None;
    }
    let audio = requested & CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE != 0;
    let video = requested & CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE != 0;
    match (video, audio) {
        (true, true) => Some(PERMISSION_KIND_CAMERA_MICROPHONE),
        (true, false) => Some(PERMISSION_KIND_CAMERA),
        (false, true) => Some(PERMISSION_KIND_MICROPHONE),
        (false, false) => None,
    }
}

/// CEF `on_request_media_access_permission(self, browser, frame, requesting_origin,
/// requested_permissions, callback) -> int` — a page's getUserMedia camera/mic
/// request. Device camera/mic requests are bridged to the shell's session-only
/// permission prompt. Desktop capture remains out of scope and returns 0 (default
/// handling / deny).
unsafe extern "C" fn on_request_media_access_permission(
    self_: *mut c_void,
    _browser: *mut c_void,
    _frame: *mut c_void,
    requesting_origin: *const CefString,
    requested_permissions: u32,
    callback: *mut c_void,
) -> c_int {
    let origin = cef_string_to_string(requesting_origin);
    with_state(self_, |state| {
        state.begin_media_access_permission(origin, requested_permissions, callback)
    })
    .unwrap_or(0)
}

/// CEF `on_show_permission_prompt(self, browser, prompt_id, requesting_origin,
/// requested_permissions, callback) -> int` — a page requested one or more
/// permissions. Map the requested types to our wire `kind`; if none are in scope
/// return 0 (default handling → Alloy deny). For an in-scope kind, add_ref + stash
/// the callback under `prompt_id`, emit `EventMsg::PermissionRequest`, and return 1
/// (handled) — the shell answers later via `ControlMsg::PermissionDecision`.
unsafe extern "C" fn on_show_permission_prompt(
    self_: *mut c_void,
    _browser: *mut c_void,
    prompt_id: u64,
    requesting_origin: *const CefString,
    requested_permissions: u32,
    callback: *mut c_void,
) -> c_int {
    let origin = cef_string_to_string(requesting_origin);
    with_state(self_, |state| {
        state.begin_permission_prompt(prompt_id, origin, requested_permissions, callback)
    })
    .unwrap_or(0)
}

/// CEF `on_dismiss_permission_prompt(self, browser, prompt_id, result)` — a prompt
/// that `on_show_permission_prompt` returned 1 for was dismissed. If we already
/// answered it (via `apply_permission_decision`), the id is gone → no-op. If CEF
/// dismissed it itself (navigation / browser closure) before the shell answered, we
/// drop our stash ref here so the added reference is not leaked. We never call
/// `cont` from here (CEF owns the dismissal).
unsafe extern "C" fn on_dismiss_permission_prompt(
    self_: *mut c_void,
    _browser: *mut c_void,
    prompt_id: u64,
    _result: c_int,
) {
    let _ = with_state(self_, |state| state.discard_permission_prompt(prompt_id));
}

/// CEF `get_audio_parameters(self, browser, params) -> int` — CEF asks whether we
/// want the page's audio stream and, if so, in what format. Returning 0 makes CEF
/// skip audio delivery entirely (the started/stopped callbacks never fire), so we
/// return 1 (true) and fill a sane STEREO / 48 kHz / 1024-frame default. We never
/// consume the samples — this only unlocks the audible-state signalling.
unsafe extern "C" fn get_audio_parameters(
    _self: *mut c_void,
    _browser: *mut c_void,
    params: *mut CefAudioParameters,
) -> c_int {
    if !params.is_null() {
        // SAFETY: CEF supplied a non-null, writable `cef_audio_parameters_t`.
        // `size` leads the struct; keeping it correct pins channel_layout to
        // offset 8, sample_rate to 12, frames_per_buffer to 16.
        unsafe {
            (*params).size = CEF_AUDIO_PARAMETERS_SIZE;
            (*params).channel_layout = CEF_CHANNEL_LAYOUT_STEREO;
            (*params).sample_rate = 48_000;
            (*params).frames_per_buffer = 1024;
        }
    }
    1
}

/// CEF `on_audio_stream_started(self, browser, params, channels)` — the page began
/// producing audio. Publish `AudioState { audible: true }` for the 🔊 tab pip. We
/// deliberately ignore `params`/`channels`: only the audible bit reaches the shell.
unsafe extern "C" fn on_audio_stream_started(
    self_: *mut c_void,
    _browser: *mut c_void,
    _params: *const CefAudioParameters,
    _channels: c_int,
) {
    let _ = with_state(self_, |state| state.publish_audio_state(true));
}

/// CEF `on_audio_stream_packet(self, browser, data, frames, pts)` — a buffer of raw
/// PCM samples. Intentionally a no-op: we surface only the audible bit, never the
/// audio data itself (no capture, no forwarding).
unsafe extern "C" fn on_audio_stream_packet(
    _self: *mut c_void,
    _browser: *mut c_void,
    _data: *const c_void,
    _frames: c_int,
    _pts: i64,
) {
}

/// CEF `on_audio_stream_stopped(self, browser)` — the page's audio stream ended.
/// Publish `AudioState { audible: false }` so the shell clears the 🔊 tab pip.
unsafe extern "C" fn on_audio_stream_stopped(self_: *mut c_void, _browser: *mut c_void) {
    let _ = with_state(self_, |state| state.publish_audio_state(false));
}

/// CEF `on_audio_stream_error(self, browser, message)` — the audio stream failed.
/// No-op: CEF stops the stream itself (a paired `on_audio_stream_stopped` clears
/// the pip); we do not surface the error text.
unsafe extern "C" fn on_audio_stream_error(
    _self: *mut c_void,
    _browser: *mut c_void,
    _message: *const CefString,
) {
}

/// CEF `on_jsdialog(self, browser, origin_url, dialog_type, message_text,
/// default_prompt_text, callback, suppress_message)` — the page called
/// `alert()`/`confirm()`/`prompt()`. v1: emit a non-blocking notice to the shell
/// and resolve the dialog synchronously so the page never blocks. `alert`
/// (type 0) is accepted (`cont(1, …)`); `confirm`/`prompt` are auto-cancelled
/// (`cont(0, …)`, safe default). No callback retention, no shell round-trip. We
/// set `*suppress_message = 0` (do not let CEF pop its own native dialog) and
/// return 1 (handled).
unsafe extern "C" fn on_jsdialog(
    self_: *mut c_void,
    _browser: *mut c_void,
    origin_url: *const CefString,
    dialog_type: c_int,
    message_text: *const CefString,
    _default_prompt_text: *const CefString,
    callback: *mut c_void,
    suppress_message: *mut c_int,
) -> c_int {
    let origin = cef_string_to_string(origin_url);
    let message = cef_string_to_string(message_text);
    let kind = u8::try_from(dialog_type).unwrap_or(0);
    let _ = with_state(self_, |state| {
        if publish_internal_page_beacon_from_dialog(state, &message) {
            return;
        }
        state.publish_js_dialog(kind, message, origin)
    });
    // Auto-resolve: accept an alert, cancel a confirm/prompt (never "OK" a
    // confirm the user never saw). Null user_input == empty prompt text.
    let success: c_int = c_int::from(dialog_type == 0);
    continue_jsdialog_callback(callback, success);
    if !suppress_message.is_null() {
        // SAFETY: CEF passes a writable `int*` for the callback duration; 0 means
        // "do not suppress our handling" — CEF must not raise its own dialog.
        unsafe { *suppress_message = 0 };
    }
    1
}

fn publish_internal_page_beacon_from_dialog(state: &CefBrowserState, message: &str) -> bool {
    if let Some((id, text)) = decode_page_text_beacon(message) {
        state.publish_page_text(id, text);
        return true;
    }
    if let Some((id, body)) = decode_page_scrape_beacon(message) {
        state.publish_page_scrape(id, body);
        return true;
    }
    false
}

/// CEF `on_before_unload_dialog(self, browser, message_text, is_reload, callback)
/// -> int` — a page registered a `beforeunload` handler and navigation/reload is
/// trying to leave. We handle it asynchronously through the shell, which must send
/// `ControlMsg::BeforeUnloadDecision`; the callback is retained until then.
unsafe extern "C" fn on_before_unload_dialog(
    self_: *mut c_void,
    _browser: *mut c_void,
    message_text: *const CefString,
    is_reload: c_int,
    callback: *mut c_void,
) -> c_int {
    let message = cef_string_to_string(message_text);
    with_state(self_, |state| {
        state.begin_before_unload_dialog(message, is_reload != 0, callback)
    })
    .unwrap_or(0)
}

/// Read a `cef_string_userfree_t`-returning getter at `offset` off a CEF object,
/// copying then freeing with the matching libcef symbol (mirrors `request_url`).
fn download_item_string(
    item: *mut c_void,
    offset: usize,
    string_userfree_free: CefStringUserfreeUtf16Free,
) -> Option<String> {
    let getter = read_fn(item, offset)?;
    // SAFETY: each named getter's pinned C signature is
    // `cef_string_userfree_t (*)(cef_download_item_t*)`.
    let getter: unsafe extern "C" fn(*mut c_void) -> *mut CefString =
        unsafe { std::mem::transmute(getter) };
    // SAFETY: CEF supplied a live download item for the callback duration.
    let raw = unsafe { getter(item) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: CEF returned a non-null userfree UTF-16 string; copy before freeing.
    let text = unsafe {
        let value = if (*raw).str_.is_null() || (*raw).length == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(std::slice::from_raw_parts((*raw).str_, (*raw).length))
        };
        string_userfree_free(raw.cast());
        value
    };
    Some(text)
}

/// CEF `can_download(self, browser, url, request_method)` — allow the flow to
/// reach on_before_download (return 1). We intercept there, not here.
unsafe extern "C" fn can_download(
    _self: *mut c_void,
    _browser: *mut c_void,
    _url: *const CefString,
    _request_method: *const CefString,
) -> c_int {
    1
}

/// CEF `on_before_download(self, browser, item, suggested_name, callback)` — a
/// download is about to start. B2: forward the URL + name to the shell (which
/// submits a daemon Transfers job) and do NOT call `callback.cont()`, so CEF
/// cancels its own (sandbox-unwritable) write. Return 0 = handled.
unsafe extern "C" fn on_before_download(
    self_: *mut c_void,
    _browser: *mut c_void,
    download_item: *mut c_void,
    suggested_name: *const CefString,
    _callback: *mut c_void,
) -> c_int {
    let filename = if suggested_name.is_null() {
        String::new()
    } else {
        cef_string_to_string(suggested_name)
    };
    let _ = with_state(self_, |state| {
        let url = download_item_string(
            download_item,
            CEF_DOWNLOAD_ITEM_GET_URL_OFFSET,
            state.string_userfree_free,
        )
        .unwrap_or_default();
        if !url.is_empty() {
            state.publish_download_intercepted(url, filename.clone());
        }
    });
    0
}

/// CEF `on_find_result(self, browser, identifier, count, rect, active_ordinal,
/// final_update)` — the native find reported its match tally.
unsafe extern "C" fn on_find_result(
    self_: *mut c_void,
    _browser: *mut c_void,
    _identifier: c_int,
    count: c_int,
    _selection_rect: *const CefRect,
    active_match_ordinal: c_int,
    final_update: c_int,
) {
    let count = u32::try_from(count).unwrap_or(0);
    let active = u32::try_from(active_match_ordinal).unwrap_or(0);
    let _ = with_state(self_, |state| {
        state.publish_find_result(count, active, final_update != 0);
    });
}

/// CEF `on_address_change(self, browser, frame, url)` — the committed URL changed.
unsafe extern "C" fn on_address_change(
    self_: *mut c_void,
    browser: *mut c_void,
    frame: *mut c_void,
    url: *const CefString,
) {
    if !frame_is_main(browser, frame) {
        return;
    }
    let url = cef_string_to_string(url);
    // Off-by-default live diagnostic: proves the display handler is actually being
    // dispatched by CEF on the real vtable (the class of bug fixed in the
    // display/load/download handler resolution). Set MDE_CEF_TRACE_NAV=1 to emit.
    if std::env::var_os("MDE_CEF_TRACE_NAV").is_some() {
        eprintln!("MDE_TRACE on_address_change url={url}");
    }
    let _ = with_state(self_, |state| {
        if let Ok(mut current) = state.nav_url.lock() {
            *current = url;
        }
        state.publish_nav_state();
    });
}

/// CEF `on_title_change(self, browser, title)` — the page title changed.
unsafe extern "C" fn on_title_change(
    self_: *mut c_void,
    _browser: *mut c_void,
    title: *const CefString,
) {
    let title = cef_string_to_string(title);
    let _ = with_state(self_, |state| state.publish_title(title));
}

/// CEF `on_favicon_urlchange(self, browser, icon_urls)` — the page's favicon URLs
/// changed. Kick off a `download_image` fetch of the first URL; the PNG arrives
/// asynchronously in [`on_download_image_finished`].
unsafe extern "C" fn on_favicon_urlchange(
    self_: *mut c_void,
    browser: *mut c_void,
    icon_urls: *mut c_void,
) {
    let _ = with_state(self_, |state| state.request_favicon(browser, icon_urls));
}

/// Map a `cef_cursor_type_t` (CEF 149) onto the engine-neutral [`CursorKind`].
fn cursor_kind_for_cef_type(cef_type: c_int) -> CursorKind {
    match cef_type {
        2 => CursorKind::Pointer,   // CT_HAND
        3 => CursorKind::Text,      // CT_IBEAM
        1 => CursorKind::Crosshair, // CT_CROSS
        4 => CursorKind::Wait,      // CT_WAIT
        5 => CursorKind::Help,      // CT_HELP
        // Resize family (CT_*RESIZE = 6..=17), column/row = 18/19.
        6 | 13 | 24 | 26 => CursorKind::ResizeHorizontal, // E/W/EASTWEST
        7 | 10 | 14 | 25 => CursorKind::ResizeVertical,   // N/S/NORTHSOUTH
        8 | 12 | 16 => CursorKind::ResizeNeSw,            // NE/SW/NESW
        9 | 11 | 17 => CursorKind::ResizeNwSe,            // NW/SE/NWSE
        18 => CursorKind::ResizeHorizontal,               // CT_COLUMNRESIZE
        19 => CursorKind::ResizeVertical,                 // CT_ROWRESIZE
        20..=28 => CursorKind::Grabbing,                  // panning
        29 => CursorKind::Move,                           // CT_MOVE
        30 => CursorKind::Text,                           // CT_VERTICALTEXT
        34 => CursorKind::Progress,                       // CT_PROGRESS
        35 | 38 => CursorKind::NotAllowed,                // CT_NODROP / CT_NOTALLOWED
        39 => CursorKind::ZoomIn,                         // CT_ZOOMIN
        40 => CursorKind::ZoomOut,                        // CT_ZOOMOUT
        41 => CursorKind::Grab,                           // CT_GRAB
        42 => CursorKind::Grabbing,                       // CT_GRABBING
        // CT_POINTER (0), and everything else CEF-neutral (cell, contextmenu,
        // alias, copy, none, dnd, custom, …) map to the plain arrow.
        _ => CursorKind::Default,
    }
}

/// CEF `on_cursor_change(self, browser, cursor, type, custom_info)` — the engine
/// changed the cursor (hover over a link, text field, resize edge, …). Forward
/// the neutral shape so the shell reflects it. Return 1 = handled.
unsafe extern "C" fn on_cursor_change(
    self_: *mut c_void,
    _browser: *mut c_void,
    _cursor: *mut c_void,
    cef_type: c_int,
    _custom_cursor_info: *const c_void,
) -> c_int {
    let kind = cursor_kind_for_cef_type(cef_type);
    let _ = with_state(self_, |state| state.publish_cursor(kind));
    1
}

/// CEF `on_fullscreen_mode_change(self, browser, fullscreen)` — the page entered or
/// left HTML5 fullscreen (`element.requestFullscreen()` / exit). Forward the state so
/// the shell hides its chrome and shows the page edge-to-edge.
unsafe extern "C" fn on_fullscreen_mode_change(
    self_: *mut c_void,
    _browser: *mut c_void,
    fullscreen: c_int,
) {
    let _ = with_state(self_, |state| state.publish_fullscreen(fullscreen != 0));
}

/// CEF `on_loading_state_change(self, browser, isLoading, canGoBack, canGoForward)`
/// — the load/back/forward edges changed; combine with the stored URL into NavState.
unsafe extern "C" fn on_loading_state_change(
    self_: *mut c_void,
    _browser: *mut c_void,
    is_loading: c_int,
    can_go_back: c_int,
    can_go_forward: c_int,
) {
    let _ = with_state(self_, |state| {
        state.nav_loading.store(is_loading != 0, Ordering::SeqCst);
        state.nav_can_back.store(can_go_back != 0, Ordering::SeqCst);
        state
            .nav_can_forward
            .store(can_go_forward != 0, Ordering::SeqCst);
        state.publish_nav_state();
        // Probe-only liveness line (no production journal noise): proves the CEF
        // load handler is registered + firing with real state during a probe run.
        if std::env::var_os("MDE_CEF_BROWSER_PROBE").is_some() {
            eprintln!(
                "CEF_NAV loading={} back={} forward={}",
                is_loading != 0,
                can_go_back != 0,
                can_go_forward != 0
            );
        }
    });
}

unsafe extern "C" fn on_print_dialog(
    _self: *mut c_void,
    _browser: *mut c_void,
    _has_selection: c_int,
    _callback: *mut c_void,
) -> c_int {
    0
}

unsafe extern "C" fn on_print_job(
    _self: *mut c_void,
    _browser: *mut c_void,
    _document_name: *const c_void,
    _pdf_file_path: *const c_void,
    _callback: *mut c_void,
) -> c_int {
    0
}

unsafe extern "C" fn get_pdf_paper_size(
    _self: *mut c_void,
    _browser: *mut c_void,
    device_units_per_inch: c_int,
) -> CefSize {
    let dpi = device_units_per_inch.max(1);
    CefSize {
        width: dpi.saturating_mul(85) / 10,
        height: dpi.saturating_mul(11),
    }
}

fn pdf_file_looks_written(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && magic == *b"%PDF"
}

unsafe extern "C" fn on_pdf_print_finished(self_: *mut c_void, path: *const c_void, ok: c_int) {
    let path = cef_string_to_string(path.cast::<CefString>());
    let _ = with_state(self_, |state| {
        state.publish_pdf_finished(path, ok != 0);
        state.finish_pdf_callback(self_);
    });
}

/// CEF `on_download_image_finished(self, image_url, http_status_code, image)` —
/// the favicon fetch completed. `image` may be NULL on failure. Encode it to PNG
/// and forward the bytes to the shell.
unsafe extern "C" fn on_download_image_finished(
    self_: *mut c_void,
    _image_url: *const CefString,
    _http_status_code: c_int,
    image: *mut c_void,
) {
    let png = image_as_png(image).filter(|png| !png.is_empty());
    let _ = with_state(self_, |state| {
        if let Some(png) = png {
            state.publish_favicon(png);
        }
        state.finish_download_image_callback(self_);
    });
}

/// CEF `cef_string_visitor_t::visit(self, const cef_string_t* string)` — native
/// visible text extraction for page text requests.
unsafe extern "C" fn on_page_text_visited(self_: *mut c_void, string: *const CefString) {
    let text = cef_string_to_string(string);
    let _ = with_state(self_, |state| {
        if let Some((id, max_bytes)) = state.finish_page_text_visitor(self_) {
            state.publish_page_text(id, clamp_utf8(&text, max_bytes as usize));
        }
    });
}

fn with_state<T>(key: *mut c_void, f: impl FnOnce(&CefBrowserState) -> T) -> Option<T> {
    let state_ptr = {
        let registry = registry().lock().ok()?;
        *registry.get(&(key as usize))?
    };
    let state = state_ptr as *const CefBrowserState;
    if state.is_null() {
        return None;
    }
    // SAFETY: registry entries are installed from a live `CefBrowserCallbacks`
    // and removed only when the callback object drops after CEF shutdown.
    Some(f(unsafe { &*state }))
}

fn registry() -> &'static Mutex<HashMap<usize, usize>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl CefBrowserState {
    fn life_span_ptr(&self) -> *mut c_void {
        lookup_peer(self, CEF_LIFE_SPAN_HANDLER_SIZE)
    }

    fn render_ptr(&self) -> *mut c_void {
        lookup_peer(self, CEF_RENDER_HANDLER_SIZE)
    }

    fn request_ptr(&self) -> *mut c_void {
        lookup_peer(self, CEF_REQUEST_HANDLER_SIZE)
    }

    fn resource_request_ptr(&self) -> *mut c_void {
        lookup_peer(self, CEF_RESOURCE_REQUEST_HANDLER_SIZE)
    }

    fn print_ptr(&self) -> *mut c_void {
        self.print_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }

    fn jsdialog_ptr(&self) -> *mut c_void {
        self.jsdialog_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }

    fn audio_ptr(&self) -> *mut c_void {
        self.audio_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }

    fn permission_ptr(&self) -> *mut c_void {
        self.permission_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }

    // These four resolve their handler block DIRECTLY via a cached pointer set at
    // install() time (like `print_ptr`/`jsdialog_ptr`), NOT via the size-keyed
    // `lookup_peer`. `lookup_peer` gates on `callback_size()`'s whitelist, which
    // does NOT list the display(144)/load(72)/find(48)/download(64) sizes — so
    // routing these through it returned NULL, silently disabling on_address_change/
    // title/favicon/cursor, loading-state, find results, and download interception
    // on live CEF (unit tests never exercise the real vtable). find(48) also
    // aliases pdf_print_callback(48), so a whitelist fix would mis-resolve — a
    // dedicated cached pointer is the only collision-proof answer.
    fn display_ptr(&self) -> *mut c_void {
        self.display_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }

    fn load_ptr(&self) -> *mut c_void {
        self.load_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }

    fn find_ptr(&self) -> *mut c_void {
        self.find_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }

    fn download_ptr(&self) -> *mut c_void {
        self.download_handler_ptr.load(Ordering::SeqCst) as *mut c_void
    }
}

fn lookup_peer(state: &CefBrowserState, size: usize) -> *mut c_void {
    let state_ptr = state as *const CefBrowserState as usize;
    let registry = registry().lock().expect("cef callback registry");
    registry
        .iter()
        .find_map(|(key, value)| {
            if *value == state_ptr && callback_size(*key) == Some(size) {
                Some(*key as *mut c_void)
            } else {
                None
            }
        })
        .unwrap_or(ptr::null_mut())
}

fn callback_size(key: usize) -> Option<usize> {
    // SAFETY: `key` is a registered callback block pointer. All callback blocks
    // start with a `size_t size` field per `cef_base_ref_counted_t`.
    let size = unsafe { *(key as *const usize) };
    match size {
        CEF_CLIENT_SIZE
        | CEF_LIFE_SPAN_HANDLER_SIZE
        | CEF_RENDER_HANDLER_SIZE
        | CEF_REQUEST_HANDLER_SIZE
        | CEF_RESOURCE_REQUEST_HANDLER_SIZE
        | CEF_PDF_PRINT_CALLBACK_SIZE => Some(size),
        _ => None,
    }
}

fn notify_browser_view_ready(browser: *mut c_void) {
    resize_and_invalidate_browser_view(browser);
}

fn resize_and_invalidate_browser_view(browser: *mut c_void) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    call_host_void(host, CEF_BROWSER_HOST_WAS_RESIZED_OFFSET);
    invalidate_browser_view(browser);
}

fn invalidate_browser_view(browser: *mut c_void) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    call_host_paint_type(host, CEF_BROWSER_HOST_INVALIDATE_OFFSET, PET_VIEW);
}

fn close_browser(browser: *mut c_void) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_CLOSE_BROWSER_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from `cef_browser_host_t::close_browser`, whose
    // pinned C signature is `void (*)(cef_browser_host_t*, int)`.
    let callback: unsafe extern "C" fn(*mut c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from `cef_browser_t::get_host`; force close avoids
    // unload-dialog waits during socket teardown.
    unsafe { callback(host, 1) };
}

fn close_browser_and_wait(abi: &CefAbi, browser: *mut c_void, callbacks: &CefBrowserCallbacks) {
    let closed_before = callbacks.closed();
    close_browser(browser);
    let deadline = Instant::now() + Duration::from_millis(900);
    while Instant::now() < deadline {
        abi.do_message_loop_work();
        if callbacks.closed() > closed_before {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..4 {
        abi.do_message_loop_work();
        thread::sleep(Duration::from_millis(4));
    }
}

fn browser_host(browser: *mut c_void) -> Option<*mut c_void> {
    if browser.is_null() {
        return None;
    }
    let get_host = read_fn(browser, CEF_BROWSER_GET_HOST_OFFSET)?;
    // SAFETY: `get_host` is read from a live `cef_browser_t` function slot using
    // the offset verified from the pinned CEF 149 headers.
    let get_host: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(get_host) };
    // SAFETY: CEF returned `browser` from `cef_browser_host_create_browser_sync`.
    let host = unsafe { get_host(browser) };
    (!host.is_null()).then_some(host)
}

fn call_host_void(host: *mut c_void, offset: usize) {
    let Some(callback) = read_fn(host, offset) else {
        return;
    };
    // SAFETY: `callback` is read from a live `cef_browser_host_t` function slot
    // using a header-verified offset.
    let callback: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(callback) };
    // SAFETY: CEF returned `host` from `cef_browser_t::get_host`.
    unsafe { callback(host) };
}

fn set_host_focus(host: *mut c_void, focused: bool) {
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SET_FOCUS_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from `cef_browser_host_t::set_focus`, whose
    // pinned C signature is `(cef_browser_host_t*, int)`.
    let callback: unsafe extern "C" fn(*mut c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and the focus flag is the CEF boolean int.
    unsafe { callback(host, if focused { 1 } else { 0 }) };
}

fn set_audio_muted(browser: *mut c_void, muted: bool) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SET_AUDIO_MUTED_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from `cef_browser_host_t::set_audio_muted`,
    // whose pinned C signature is `(cef_browser_host_t*, int)`.
    let callback: unsafe extern "C" fn(*mut c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and the mute flag is the CEF boolean int.
    unsafe { callback(host, if muted { 1 } else { 0 }) };
}

/// Push an IME preedit into the windowless browser via
/// `cef_browser_host_t::ime_set_composition`. We pass no underline runs and no
/// replacement range; the selection range places the caret at the end of the
/// preedit. An empty `text` clears the active composition (CEF treats a
/// zero-length composition string as a cancel).
fn ime_set_composition(browser: *mut c_void, text: &str) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Ok(composition) = CefStringOwned::new(text) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_IME_SET_COMPOSITION_OFFSET) else {
        return;
    };
    // CEF composition ranges are measured in UTF-16 code units, matching
    // `cef_string_t::length`; the caret sits at the end of the preedit.
    let caret = text.encode_utf16().count() as u32;
    let selection_range = CefRange {
        from: caret,
        to: caret,
    };
    // SAFETY: `callback` is read from `cef_browser_host_t::ime_set_composition`,
    // whose pinned C signature is `(cef_browser_host_t*, const cef_string_t*
    // text, size_t underlines_count, const cef_composition_underline_t*
    // underlines, const cef_range_t* replacement_range, const cef_range_t*
    // selection_range)`.
    let callback: unsafe extern "C" fn(
        *mut c_void,
        *const c_void,
        usize,
        *const c_void,
        *const CefRange,
        *const CefRange,
    ) = unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF; `composition` is a live `cef_string_t` for
    // this call; zero underline runs / NULL underline array; NULL
    // replacement_range; `selection_range` outlives the synchronous call.
    unsafe {
        callback(
            host,
            composition.as_ptr(),
            0,
            ptr::null(),
            ptr::null(),
            &selection_range as *const CefRange,
        )
    };
}

/// Commit finalized IME text into the browser via
/// `cef_browser_host_t::ime_commit_text` (no replacement range, cursor left at
/// the default position).
fn ime_commit_text(browser: *mut c_void, text: &str) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Ok(commit) = CefStringOwned::new(text) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_IME_COMMIT_TEXT_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from `cef_browser_host_t::ime_commit_text`, whose
    // pinned C signature is `(cef_browser_host_t*, const cef_string_t* text,
    // const cef_range_t* replacement_range, int relative_cursor_pos)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void, *const CefRange, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF; `commit` is a live `cef_string_t` for this
    // call; NULL replacement_range and a zero relative cursor position.
    unsafe { callback(host, commit.as_ptr(), ptr::null(), 0) };
}

/// Finish the active IME composition via
/// `cef_browser_host_t::ime_finish_composing_text`, keeping the current
/// selection (`keep_selection = 1`).
fn ime_finish_composing(browser: *mut c_void) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_IME_FINISH_COMPOSING_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from
    // `cef_browser_host_t::ime_finish_composing_text`, whose pinned C signature
    // is `(cef_browser_host_t*, int keep_selection)`.
    let callback: unsafe extern "C" fn(*mut c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF; keep the current selection after finishing.
    unsafe { callback(host, 1) };
}

fn print_page(browser: *mut c_void) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    call_host_void(host, CEF_BROWSER_HOST_PRINT_OFFSET);
}

fn save_pdf(browser: *mut c_void, callbacks: &CefBrowserCallbacks, path: &str) {
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Ok(path) = CefStringOwned::new(path) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_PRINT_TO_PDF_OFFSET) else {
        return;
    };
    let pdf_callback = callbacks.retain_pdf_callback();
    if pdf_callback.is_null() {
        return;
    }
    // SAFETY: `callback` is read from `cef_browser_host_t::print_to_pdf`, whose
    // pinned C signature is `(cef_browser_host_t*, const cef_string_t*,
    // const cef_pdf_print_settings_t*, cef_pdf_print_callback_t*)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void, *mut c_void) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF, `path` points to a live CefString for this
    // call, null settings asks CEF for defaults, and the callback is retained by
    // the browser state until CEF reports completion.
    unsafe { callback(host, path.as_ptr(), ptr::null(), pdf_callback) };
}

fn call_host_paint_type(host: *mut c_void, offset: usize, paint_type: c_int) {
    let Some(callback) = read_fn(host, offset) else {
        return;
    };
    // SAFETY: `callback` is read from a live `cef_browser_host_t` function slot
    // using a header-verified offset.
    let callback: unsafe extern "C" fn(*mut c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: CEF returned `host` from `cef_browser_t::get_host`; `PET_VIEW` is a
    // valid `cef_paint_element_type_t`.
    unsafe { callback(host, paint_type) };
}

fn call_browser_void(browser: *mut c_void, offset: usize) {
    let Some(callback) = read_fn(browser, offset) else {
        return;
    };
    // SAFETY: `callback` is read from a live `cef_browser_t` void method slot
    // using a header-verified offset.
    let callback: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(callback) };
    // SAFETY: CEF returned `browser` from `cef_browser_host_create_browser_sync`.
    unsafe { callback(browser) };
}

fn call_browser_bool(browser: *mut c_void, offset: usize) -> bool {
    let Some(callback) = read_fn(browser, offset) else {
        return false;
    };
    // SAFETY: `callback` is read from a live `cef_browser_t` int-returning method
    // slot using a header-verified offset.
    let callback: unsafe extern "C" fn(*mut c_void) -> c_int =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: CEF returned `browser` from `cef_browser_host_create_browser_sync`.
    unsafe { callback(browser) != 0 }
}

fn send_mouse_move(host: *mut c_void, x: i32, y: i32, modifiers: c_int, mouse_leave: bool) {
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SEND_MOUSE_MOVE_EVENT_OFFSET) else {
        return;
    };
    let event = CefMouseEvent::new(x, y, modifiers);
    // SAFETY: `callback` is read from `cef_browser_host_t::send_mouse_move_event`,
    // whose pinned C signature is `(cef_browser_host_t*, const cef_mouse_event_t*, int)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and `event` lives for the duration of the call.
    unsafe { callback(host, event.as_ptr(), if mouse_leave { 1 } else { 0 }) };
}

fn send_mouse_click(
    host: *mut c_void,
    x: i32,
    y: i32,
    button: PointerButton,
    pressed: bool,
    modifiers: c_int,
    click_count: c_int,
) {
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SEND_MOUSE_CLICK_EVENT_OFFSET) else {
        return;
    };
    // The event modifiers carry both the pressed-button flag CEF tracks AND the
    // held keyboard modifiers, so Ctrl/Shift/Cmd-click reach the page.
    let event = CefMouseEvent::new(x, y, mouse_button_event_flag(button) | modifiers);
    // SAFETY: `callback` is read from `cef_browser_host_t::send_mouse_click_event`,
    // whose pinned C signature is `(cef_browser_host_t*, const cef_mouse_event_t*,
    // cef_mouse_button_type_t, int, int)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void, c_int, c_int, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and `event` lives for the duration of the call.
    unsafe {
        callback(
            host,
            event.as_ptr(),
            cef_mouse_button(button),
            if pressed { 0 } else { 1 },
            click_count.max(1),
        )
    };
}

fn send_mouse_wheel(
    host: *mut c_void,
    x: i32,
    y: i32,
    delta_x: f32,
    delta_y: f32,
    modifiers: c_int,
) {
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SEND_MOUSE_WHEEL_EVENT_OFFSET) else {
        return;
    };
    // Held modifiers reach the page: Ctrl-wheel zoom, Shift-wheel horizontal.
    let event = CefMouseEvent::new(x, y, modifiers);
    // SAFETY: `callback` is read from `cef_browser_host_t::send_mouse_wheel_event`,
    // whose pinned C signature is `(cef_browser_host_t*, const cef_mouse_event_t*, int, int)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void, c_int, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and `event` lives for the duration of the call.
    unsafe {
        callback(
            host,
            event.as_ptr(),
            f32_to_i32(delta_x),
            f32_to_i32(delta_y),
        )
    };
}

fn send_key(host: *mut c_void, key: KeyCode, pressed: bool, modifiers: Modifiers) {
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SEND_KEY_EVENT_OFFSET) else {
        return;
    };
    let Some(code) = windows_key_code(key) else {
        return;
    };
    let event = CefKeyEvent::new(
        if pressed {
            KEYEVENT_RAWKEYDOWN
        } else {
            KEYEVENT_KEYUP
        },
        cef_modifiers(modifiers),
        code,
        code,
        0,
        0,
    );
    // SAFETY: `callback` is read from `cef_browser_host_t::send_key_event`,
    // whose pinned C signature is `(cef_browser_host_t*, const cef_key_event_t*)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and `event` lives for the duration of the call.
    unsafe { callback(host, event.as_ptr()) };
}

fn send_char(host: *mut c_void, character: u16, modifiers: Modifiers) {
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SEND_KEY_EVENT_OFFSET) else {
        return;
    };
    let key_code = char_windows_key_code(character);
    let event = CefKeyEvent::new(
        KEYEVENT_CHAR,
        cef_modifiers(modifiers),
        key_code,
        key_code,
        character,
        character,
    );
    // SAFETY: `callback` is read from `cef_browser_host_t::send_key_event`,
    // whose pinned C signature is `(cef_browser_host_t*, const cef_key_event_t*)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and `event` lives for the duration of the call.
    unsafe { callback(host, event.as_ptr()) };
}

fn char_windows_key_code(character: u16) -> i32 {
    match char::from_u32(u32::from(character)) {
        Some(ch @ 'a'..='z') => ch.to_ascii_uppercase() as i32,
        Some(ch @ ('A'..='Z' | '0'..='9')) => ch as i32,
        Some(' ') => 32,
        Some('\t') => 9,
        Some('\n' | '\r') => 13,
        _ => i32::from(character),
    }
}

fn cef_mouse_button(button: PointerButton) -> c_int {
    match button {
        PointerButton::Primary => MBT_LEFT,
        PointerButton::Secondary => MBT_RIGHT,
        PointerButton::Middle => MBT_MIDDLE,
    }
}

fn mouse_button_event_flag(button: PointerButton) -> c_int {
    match button {
        PointerButton::Primary => EVENTFLAG_LEFT_MOUSE_BUTTON,
        PointerButton::Secondary => EVENTFLAG_RIGHT_MOUSE_BUTTON,
        PointerButton::Middle => EVENTFLAG_MIDDLE_MOUSE_BUTTON,
    }
}

fn cef_modifiers(modifiers: Modifiers) -> c_int {
    let mut flags = 0;
    if modifiers.has(Modifiers::SHIFT) {
        flags |= EVENTFLAG_SHIFT_DOWN;
    }
    if modifiers.has(Modifiers::CTRL) {
        flags |= EVENTFLAG_CONTROL_DOWN;
    }
    if modifiers.has(Modifiers::ALT) {
        flags |= EVENTFLAG_ALT_DOWN;
    }
    if modifiers.has(Modifiers::COMMAND) {
        flags |= EVENTFLAG_COMMAND_DOWN;
    }
    flags
}

fn windows_key_code(key: KeyCode) -> Option<i32> {
    let code = match key {
        KeyCode::Enter => 13,
        KeyCode::Escape => 27,
        KeyCode::Backspace => 8,
        KeyCode::Tab => 9,
        KeyCode::Space => 32,
        KeyCode::Delete => 46,
        KeyCode::Insert => 45,
        KeyCode::Home => 36,
        KeyCode::End => 35,
        KeyCode::PageUp => 33,
        KeyCode::PageDown => 34,
        KeyCode::ArrowUp => 38,
        KeyCode::ArrowDown => 40,
        KeyCode::ArrowLeft => 37,
        KeyCode::ArrowRight => 39,
        KeyCode::A => 65,
        KeyCode::B => 66,
        KeyCode::C => 67,
        KeyCode::D => 68,
        KeyCode::E => 69,
        KeyCode::F => 70,
        KeyCode::G => 71,
        KeyCode::H => 72,
        KeyCode::I => 73,
        KeyCode::J => 74,
        KeyCode::K => 75,
        KeyCode::L => 76,
        KeyCode::M => 77,
        KeyCode::N => 78,
        KeyCode::O => 79,
        KeyCode::P => 80,
        KeyCode::Q => 81,
        KeyCode::R => 82,
        KeyCode::S => 83,
        KeyCode::T => 84,
        KeyCode::U => 85,
        KeyCode::V => 86,
        KeyCode::W => 87,
        KeyCode::X => 88,
        KeyCode::Y => 89,
        KeyCode::Z => 90,
        KeyCode::Num0 => 48,
        KeyCode::Num1 => 49,
        KeyCode::Num2 => 50,
        KeyCode::Num3 => 51,
        KeyCode::Num4 => 52,
        KeyCode::Num5 => 53,
        KeyCode::Num6 => 54,
        KeyCode::Num7 => 55,
        KeyCode::Num8 => 56,
        KeyCode::Num9 => 57,
        KeyCode::F1 => 112,
        KeyCode::F2 => 113,
        KeyCode::F3 => 114,
        KeyCode::F4 => 115,
        KeyCode::F5 => 116,
        KeyCode::F6 => 117,
        KeyCode::F7 => 118,
        KeyCode::F8 => 119,
        KeyCode::F9 => 120,
        KeyCode::F10 => 121,
        KeyCode::F11 => 122,
        KeyCode::F12 => 123,
    };
    Some(code)
}

fn f32_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        0
    } else if value >= i32::MAX as f32 {
        i32::MAX
    } else if value <= i32::MIN as f32 {
        i32::MIN
    } else {
        value.round() as i32
    }
}

fn load_url(browser: *mut c_void, url: &str) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    load_frame_url(frame, url);
}

fn load_frame_url(frame: *mut c_void, url: &str) {
    let Ok(url) = CefStringOwned::new(url) else {
        return;
    };
    let Some(callback) = read_fn(frame, CEF_FRAME_LOAD_URL_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from `cef_frame_t::load_url`, whose pinned C
    // signature is `void (*)(cef_frame_t*, const cef_string_t*)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `frame` came from `cef_browser_t::get_main_frame`; `url` owns the
    // UTF-16 buffer for the duration of the call.
    unsafe { callback(frame, url.as_ptr()) };
}

fn apply_cosmetic_filters(browser: *mut c_void, css: &str) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    let script = cosmetic_filter_script(css);
    execute_java_script(frame, &script);
}

fn apply_force_dark(browser: *mut c_void, enabled: bool) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &force_dark_script(enabled));
}

fn apply_reader_mode(browser: *mut c_void, enabled: bool) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &reader_mode_script(enabled));
}

fn apply_autoplay_blocked(browser: *mut c_void, state: &CefBrowserState, blocked: bool) {
    state.autoplay_blocked.store(blocked, Ordering::SeqCst);
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &autoplay_block_script(blocked));
}

fn apply_media_playback_toggle(browser: *mut c_void) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, media_playback_toggle_script());
}

fn apply_media_transport(browser: *mut c_void, action: MediaTransportAction) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &media_transport_script(action));
}

fn apply_user_scripts(browser: *mut c_void, enabled: bool, bundle: &str) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &userscript_library_script(enabled, bundle));
}

fn apply_user_agent(browser: *mut c_void, user_agent: &str) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &user_agent_override_script(user_agent));
}

/// Fill the page's login form with a user-chosen saved credential (autofill). The
/// shell scopes the credential to `expected_host`; check CEF's cached top-level
/// URL immediately before injection so a navigation race cannot fill another site.
fn fill_login(
    browser: *mut c_void,
    state: &CefBrowserState,
    expected_host: &str,
    username: &str,
    password: &str,
) {
    if !state.host_matches_top_level(expected_host) {
        return;
    }
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &login_fill_script(username, password));
}

fn apply_device_profile(
    browser: *mut c_void,
    profile: &str,
    width: u16,
    height: u16,
    scale_percent: u16,
    touch: bool,
) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(
        frame,
        &device_profile_script(profile, width, height, scale_percent, touch),
    );
}

fn apply_spellcheck_highlights(browser: *mut c_void, words: &[String]) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &spellcheck_highlight_script(words));
}

fn apply_spellcheck_correction(browser: *mut c_void, word: &str, replacement: &str) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &spellcheck_correction_script(word, replacement));
}

fn apply_spellcheck_correction_all(browser: *mut c_void, word: &str, replacement: &str) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &spellcheck_correction_all_script(word, replacement));
}

fn apply_spellcheck_correction_at(
    browser: *mut c_void,
    word: &str,
    replacement: &str,
    occurrence: u16,
) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(
        frame,
        &spellcheck_correction_at_script(word, replacement, occurrence),
    );
}

fn request_page_text(browser: *mut c_void, state: &CefBrowserState, id: u64, max_bytes: u32) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    let _ = request_native_page_text(frame, state, id, max_bytes);
    // The native CEF string visitor is best-effort in the offscreen bridge: some
    // live runtimes accept `cef_frame_t::get_text` but never call back. Always
    // send the intercepted JS beacon too so page-text requests cannot wedge
    // behind a synchronously accepted native request.
    let script = page_text_beacon_script(id, max_bytes);
    execute_java_script(frame, &script);
    load_frame_url(frame, &javascript_url_for_script(&script));
}

fn request_native_page_text(
    frame: *mut c_void,
    state: &CefBrowserState,
    id: u64,
    max_bytes: u32,
) -> bool {
    let Some(callback) = read_fn(frame, CEF_FRAME_GET_TEXT_OFFSET) else {
        return false;
    };
    let visitor = state.retain_page_text_visitor(id, max_bytes);
    if visitor.is_null() {
        return false;
    }
    // SAFETY: `callback` is `cef_frame_t::get_text`, whose pinned C signature is
    // `void (*)(cef_frame_t*, cef_string_visitor_t*)`.
    let callback: unsafe extern "C" fn(*mut c_void, *mut c_void) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `frame` is the live main frame and `visitor` is retained in
    // `CefBrowserState` until CEF invokes the one-shot visit callback.
    unsafe { callback(frame, visitor) };
    true
}

fn request_page_scrape(
    browser: *mut c_void,
    id: u64,
    max_bytes: u32,
    max_links: u16,
    max_headings: u16,
) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(
        frame,
        &page_scrape_beacon_script(id, max_bytes, max_links, max_headings),
    );
}

/// Inject the per-context document shims into the current document (browser-8).
/// Called once per navigation generation and through a fresh document's settle
/// window — not on a wall-clock timer.
fn inject_context_shims(browser: *mut c_void, state: &CefBrowserState) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    if state.webrtc_blocked.load(Ordering::SeqCst) {
        execute_java_script(frame, webrtc_block_script());
    }
    execute_java_script(frame, &passkey_bridge_script());
    execute_java_script(frame, &login_capture_script());
    if state.autoplay_blocked.load(Ordering::SeqCst) {
        execute_java_script(frame, &autoplay_block_script(true));
    }
}

/// Drain any page-initiated passkey ceremonies queued since the last tick. This
/// is a lightweight heartbeat (it just calls the installed
/// `__mdeBrowserPasskeyDrain` closure) rather than recompiling the multi-KB
/// bridge shim every 250 ms.
fn poll_passkey_drain(browser: *mut c_void) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, passkey_drain_script());
}

fn poll_media_metadata(browser: *mut c_void) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &media_metadata_beacon_script());
}

fn complete_passkey(browser: *mut c_void, body: &str) {
    let Some(frame) = main_frame(browser) else {
        return;
    };
    execute_java_script(frame, &passkey_complete_script(body));
}

/// Chromium's zoom convention: `zoom_level = ln(factor) / ln(1.2)` (one level ≈
/// one Ctrl+/- step). Clamped to the same 25–500% range the shell offers.
fn zoom_level_for_percent(percent: u16) -> f64 {
    let clamped = percent.clamp(25, 500);
    (f64::from(clamped) / 100.0).ln() / 1.2f64.ln()
}

fn apply_page_zoom(browser: *mut c_void, percent: u16) {
    // Native browser zoom (layout + images + fixed elements), not a CSS `zoom`
    // property injection — matches Ctrl+/- semantics in a desktop browser.
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_SET_ZOOM_LEVEL_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from `cef_browser_host_t::set_zoom_level`, whose
    // pinned C signature is `(cef_browser_host_t*, double)`.
    let callback: unsafe extern "C" fn(*mut c_void, f64) = unsafe { std::mem::transmute(callback) };
    // SAFETY: `host` came from CEF and remains valid for the call.
    unsafe { callback(host, zoom_level_for_percent(percent)) };
}

fn apply_find_in_page(browser: *mut c_void, query: &str, backwards: bool, find_next: bool) {
    if query.trim().is_empty() {
        clear_find_in_page(browser);
        return;
    }
    // Native host->find: highlights ALL matches, cycles with find_next, and fires
    // on_find_result with the match tally (unlike the old window.find script,
    // which had no count). forward = !backwards; case-insensitive.
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_FIND_OFFSET) else {
        return;
    };
    let Ok(search) = CefStringOwned::new(query) else {
        return;
    };
    // SAFETY: `callback` is `cef_browser_host_t::find`, signature
    // `(host*, const cef_string_t*, int forward, int match_case, int find_next)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void, c_int, c_int, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: host + the owned search string live for the duration of the call.
    unsafe {
        callback(
            host,
            search.as_ptr(),
            c_int::from(!backwards),
            0,
            c_int::from(find_next),
        );
    }
}

/// Run a native `cef_frame_t` edit command on the main frame's focused element.
fn apply_edit_command(browser: *mut c_void, command: EditCommand) {
    let offset = match command {
        EditCommand::Undo => CEF_FRAME_UNDO_OFFSET,
        EditCommand::Redo => CEF_FRAME_REDO_OFFSET,
        EditCommand::Cut => CEF_FRAME_CUT_OFFSET,
        EditCommand::Copy => CEF_FRAME_COPY_OFFSET,
        EditCommand::Paste => CEF_FRAME_PASTE_OFFSET,
        EditCommand::Delete => CEF_FRAME_DELETE_OFFSET,
        EditCommand::SelectAll => CEF_FRAME_SELECT_ALL_OFFSET,
    };
    let Some(frame) = main_frame(browser) else {
        return;
    };
    let Some(callback) = read_fn(frame, offset) else {
        return;
    };
    // SAFETY: every cef_frame_t edit command is `void method(cef_frame_t* self)`.
    let callback: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(callback) };
    // SAFETY: `frame` came from get_main_frame and is valid for the call.
    unsafe { callback(frame) };
}

fn clear_find_in_page(browser: *mut c_void) {
    // Native stop_finding(clear_selection=1) — ends the search + drops highlights.
    let Some(host) = browser_host(browser) else {
        return;
    };
    let Some(callback) = read_fn(host, CEF_BROWSER_HOST_STOP_FINDING_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is `cef_browser_host_t::stop_finding`, signature
    // `(host*, int clear_selection)`.
    let callback: unsafe extern "C" fn(*mut c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: host came from CEF and remains valid for the call.
    unsafe { callback(host, 1) };
}

fn execute_java_script(frame: *mut c_void, script: &str) {
    let Ok(script) = CefStringOwned::new(script) else {
        return;
    };
    let Ok(script_url) = CefStringOwned::new("mde://cosmetic-filters") else {
        return;
    };
    let Some(callback) = read_fn(frame, CEF_FRAME_EXECUTE_JAVA_SCRIPT_OFFSET) else {
        return;
    };
    // SAFETY: `callback` is read from `cef_frame_t::execute_java_script`, whose
    // pinned C signature is
    // `void (*)(cef_frame_t*, const cef_string_t*, const cef_string_t*, int)`.
    let callback: unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void, c_int) =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: `frame` came from `cef_browser_t::get_main_frame`; both CEF strings
    // own their UTF-16 buffers for the duration of the call.
    unsafe { callback(frame, script.as_ptr(), script_url.as_ptr(), 1) };
}

fn decode_page_text_beacon(url: &str) -> Option<(u64, String)> {
    let rest = url
        .strip_prefix(CEF_PAGE_TEXT_BEACON_PREFIX)
        .or_else(|| url.strip_prefix(CEF_PAGE_TEXT_BEACON_LEGACY_PREFIX))?;
    let (id, query) = rest.split_once('?').unwrap_or((rest, ""));
    let id = id.parse::<u64>().ok()?;
    let text = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("text="))
        .unwrap_or_default();
    Some((
        id,
        clamp_utf8(
            &percent_decode(text),
            CEF_PAGE_TEXT_BEACON_MAX_BYTES as usize,
        ),
    ))
}

fn decode_page_scrape_beacon(url: &str) -> Option<(u64, String)> {
    let rest = url.strip_prefix(CEF_PAGE_SCRAPE_BEACON_PREFIX)?;
    let (id, query) = rest.split_once('?').unwrap_or((rest, ""));
    let id = id.parse::<u64>().ok()?;
    let body = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("body="))
        .unwrap_or_default();
    Some((
        id,
        clamp_utf8(&percent_decode(body), CEF_PAGE_SCRAPE_BEACON_MAX_BYTES),
    ))
}

fn decode_passkey_beacon(url: &str) -> Option<String> {
    let query = url.strip_prefix(CEF_PASSKEY_BEACON_PREFIX)?;
    let query = query.strip_prefix('?').unwrap_or(query);
    let body = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("body="))
        .unwrap_or_default();
    let body = clamp_utf8(&percent_decode(body), CEF_PASSKEY_BEACON_MAX_BYTES);
    body.trim_start().starts_with('{').then_some(body)
}

fn decode_media_metadata_beacon(url: &str) -> Option<String> {
    let query = url.strip_prefix(CEF_MEDIA_METADATA_BEACON_PREFIX)?;
    let query = query.strip_prefix('?').unwrap_or(query);
    let body = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("body="))
        .unwrap_or_default();
    let body = clamp_utf8(&percent_decode(body), CEF_MEDIA_METADATA_BEACON_MAX_BYTES);
    if body.is_empty() {
        return Some(body);
    }
    body.trim_start().starts_with('{').then_some(body)
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

/// Decode a login-capture beacon URL into `(origin, body)`. Returns `None` for
/// malformed login beacons; callers still cancel every URL with the login prefix so
/// bad attempts never reach the network/resource filter path.
fn decode_login_beacon(url: &str) -> Option<(String, String)> {
    let query = url.strip_prefix(CEF_LOGIN_BEACON_PREFIX)?;
    let query = query.strip_prefix('?').unwrap_or(query);
    let origin = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("origin="))
        .unwrap_or_default();
    let body = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("body="))
        .unwrap_or_default();
    let origin = clamp_utf8(&percent_decode(origin), 512);
    let body = clamp_utf8(&percent_decode(body), CEF_LOGIN_BEACON_MAX_BYTES);
    (credential_host(&origin).is_some() && body.trim_start().starts_with('{'))
        .then_some((origin, body))
}

fn hosts_match(left: &str, right: &str) -> bool {
    credential_host(left)
        .zip(credential_host(right))
        .is_some_and(|(l, r)| l == r)
}

fn credential_host(value: &str) -> Option<String> {
    host_of_url(value).or_else(|| {
        let host = value
            .trim()
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        (!host.is_empty() && !host.contains('/') && !host.contains('?') && !host.contains('#'))
            .then_some(host)
    })
}

fn host_of_url(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = hostport.strip_prefix('[').map_or_else(
        || hostport.split_once(':').map_or(hostport, |(h, _)| h),
        |rest| rest.split_once(']').map_or(rest, |(h, _)| h),
    );
    if host.is_empty() {
        None
    } else {
        Some(host.trim_end_matches('.').to_ascii_lowercase())
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_url_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(char::from(byte)),
            _ => {
                out.push('%');
                out.push(char::from(HEX[(byte >> 4) as usize]));
                out.push(char::from(HEX[(byte & 0x0f) as usize]));
            }
        }
    }
    out
}

fn javascript_url_for_script(script: &str) -> String {
    format!("javascript:{}", percent_encode_url_component(script))
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn clamp_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn js_string_literal(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if ch <= '\u{1f}' => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn js_string_array_literal(values: &[String]) -> String {
    let mut out = String::from("[");
    for (idx, value) in values.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&js_string_literal(value));
    }
    out.push(']');
    out
}

fn main_frame(browser: *mut c_void) -> Option<*mut c_void> {
    let get_main_frame = read_fn(browser, CEF_BROWSER_GET_MAIN_FRAME_OFFSET)?;
    // SAFETY: `get_main_frame` is read from a live `cef_browser_t` function slot
    // using the offset verified from the pinned CEF 149 headers.
    let get_main_frame: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(get_main_frame) };
    // SAFETY: CEF returned `browser` from `cef_browser_host_create_browser_sync`.
    let frame = unsafe { get_main_frame(browser) };
    (!frame.is_null()).then_some(frame)
}

fn frame_is_main(browser: *mut c_void, frame: *mut c_void) -> bool {
    !browser.is_null()
        && !frame.is_null()
        && main_frame(browser).is_some_and(|main| std::ptr::eq(main, frame))
}

fn request_url(
    request: *mut c_void,
    string_userfree_free: CefStringUserfreeUtf16Free,
) -> Option<String> {
    let get_url = read_fn(request, CEF_REQUEST_GET_URL_OFFSET)?;
    // SAFETY: `get_url` is read from `cef_request_t::get_url`, whose pinned C
    // signature is `cef_string_userfree_t (*)(cef_request_t*)`.
    let get_url: unsafe extern "C" fn(*mut c_void) -> *mut CefString =
        unsafe { std::mem::transmute(get_url) };
    // SAFETY: CEF supplied a live `cef_request_t` for the callback duration.
    let raw = unsafe { get_url(request) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: CEF returned a non-null userfree UTF-16 string. Copy before
    // freeing with the matching libcef symbol.
    let text = unsafe {
        let value = if (*raw).str_.is_null() || (*raw).length == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(std::slice::from_raw_parts((*raw).str_, (*raw).length))
        };
        string_userfree_free(raw.cast());
        value
    };
    (!text.is_empty()).then_some(text)
}

/// Stamp an HTTP request header onto a live, mutable `cef_request_t` via
/// `set_header_by_name(name, value, overwrite=1)`. Used to override the real
/// `User-Agent:` header so server-side sniffers match the JS `navigator.userAgent`
/// shim. No-op if the request/slot is null or the header text contains a NUL. Only
/// safe to call on the mutable request CEF hands to `on_before_resource_load`.
fn set_request_header(request: *mut c_void, name: &str, value: &str) {
    let (Ok(name), Ok(value)) = (CefStringOwned::new(name), CefStringOwned::new(value)) else {
        return;
    };
    let Some(set_header) = read_fn(request, CEF_REQUEST_SET_HEADER_BY_NAME_OFFSET) else {
        return;
    };
    // SAFETY: `set_header` is read from `cef_request_t::set_header_by_name`, whose
    // pinned C signature is `void (*)(cef_request_t*, const cef_string_t* name,
    // const cef_string_t* value, int overwrite)`.
    let set_header: unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void, c_int) =
        unsafe { std::mem::transmute(set_header) };
    // SAFETY: CEF passes a live, mutable `cef_request_t` to on_before_resource_load;
    // `name`/`value` own their UTF-16 buffers for the duration of the call.
    // overwrite = 1 replaces the default header CEF already populated.
    unsafe { set_header(request, name.as_ptr(), value.as_ptr(), 1) };
}

fn cef_string_to_string(raw: *const CefString) -> String {
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: callers pass a CEF-owned string pointer that is live for the
    // callback duration. Copy the UTF-16 contents immediately.
    unsafe {
        if (*raw).str_.is_null() || (*raw).length == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(std::slice::from_raw_parts((*raw).str_, (*raw).length))
        }
    }
}

/// Encode a `cef_image_t*` to PNG bytes via `get_as_png` (scale 1.0, with
/// transparency) and the returned `cef_binary_value_t`. Returns `None` if the
/// image is NULL or yields no PNG representation. The image's ref-count is NOT
/// touched (CEF owns it, borrowed for the callback); the returned binary value is
/// released after its bytes are copied out.
fn image_as_png(image: *mut c_void) -> Option<Vec<u8>> {
    if image.is_null() {
        return None;
    }
    let get_as_png = read_fn(image, CEF_IMAGE_GET_AS_PNG_OFFSET)?;
    // SAFETY: read from `cef_image_t::get_as_png`, whose pinned C signature is
    // `cef_binary_value_t* (cef_image_t*, float, int, int*, int*)`.
    let get_as_png: unsafe extern "C" fn(
        *mut c_void,
        f32,
        c_int,
        *mut c_int,
        *mut c_int,
    ) -> *mut c_void = unsafe { std::mem::transmute(get_as_png) };
    let mut pixel_width: c_int = 0;
    let mut pixel_height: c_int = 0;
    // SAFETY: `image` is a live CEF image for the callback; the out-params are
    // stack ints written by CEF.
    let binary = unsafe { get_as_png(image, 1.0, 1, &mut pixel_width, &mut pixel_height) };
    if binary.is_null() {
        return None;
    }
    let png = read_binary_value(binary);
    release_cef(binary);
    png
}

/// Copy a `cef_binary_value_t`'s bytes out via `get_size` + `get_data`.
fn read_binary_value(binary: *mut c_void) -> Option<Vec<u8>> {
    let get_size = read_fn(binary, CEF_BINARY_VALUE_GET_SIZE_OFFSET)?;
    // SAFETY: read from `cef_binary_value_t::get_size`, sig `size_t (self)`.
    let get_size: unsafe extern "C" fn(*mut c_void) -> usize =
        unsafe { std::mem::transmute(get_size) };
    // SAFETY: `binary` is the live value returned by `get_as_png`.
    let size = unsafe { get_size(binary) };
    if size == 0 {
        return None;
    }
    let get_data = read_fn(binary, CEF_BINARY_VALUE_GET_DATA_OFFSET)?;
    // SAFETY: read from `cef_binary_value_t::get_data`, sig
    // `size_t (self, void* buffer, size_t buffer_size, size_t data_offset)`.
    let get_data: unsafe extern "C" fn(*mut c_void, *mut c_void, usize, usize) -> usize =
        unsafe { std::mem::transmute(get_data) };
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` holds `size` writable bytes; CEF copies at most `size` from
    // offset 0.
    let copied = unsafe { get_data(binary, buf.as_mut_ptr().cast(), size, 0) };
    buf.truncate(copied.min(size));
    (!buf.is_empty()).then_some(buf)
}

/// Free a `cef_string_t` that `cef_string_list_value` populated with an owning
/// heap copy: invoke its `dtor` on the backing buffer, then clear the fields so a
/// double free is impossible.
fn free_cef_string_copy(s: &mut CefString) {
    if s.dtor != 0 && !s.str_.is_null() {
        // SAFETY: `cef_string_list_value` set `dtor` to libcef's matching free
        // function for the `str_` buffer it allocated. Call it exactly once.
        let dtor: unsafe extern "C" fn(*mut u16) = unsafe { std::mem::transmute(s.dtor) };
        unsafe { dtor(s.str_ as *mut u16) };
    }
    s.str_ = ptr::null();
    s.length = 0;
    s.dtor = 0;
}

fn add_ref_cef(object: *mut c_void) {
    call_ref_counted_void(object, BASE_ADD_REF_OFFSET);
}

fn release_cef(object: *mut c_void) {
    call_ref_counted_int(object, BASE_RELEASE_OFFSET);
}

fn continue_cef_callback(callback: *mut c_void) {
    call_ref_counted_void(callback, CEF_CALLBACK_CONT_OFFSET);
}

fn cancel_cef_callback(callback: *mut c_void) {
    call_ref_counted_void(callback, CEF_CALLBACK_CANCEL_OFFSET);
}

/// Invoke `cef_jsdialog_callback_t::cont(self, success, user_input)`. Unlike the
/// download/resource `cont` (which takes only `self`), the jsdialog callback
/// carries `int success` + a `const cef_string_t* user_input`; we pass a null
/// string (empty input). No-op if the slot is null.
fn continue_jsdialog_callback(callback: *mut c_void, success: c_int) {
    let Some(cont) = read_fn(callback, CEF_CALLBACK_CONT_OFFSET) else {
        return;
    };
    // SAFETY: read from `cef_jsdialog_callback_t::cont`, whose pinned C signature
    // is `void (*)(cef_jsdialog_callback_t*, int, const cef_string_t*)`.
    let cont: unsafe extern "C" fn(*mut c_void, c_int, *const CefString) =
        unsafe { std::mem::transmute(cont) };
    // SAFETY: `callback` is the live CEF callback for this invocation; a null
    // `cef_string_t*` means empty user input (accepted by CEF).
    unsafe { cont(callback, success, ptr::null()) };
}

/// Invoke `cef_permission_prompt_callback_t::cont(self, result)`. `result` is a
/// `cef_permission_request_result_t` (int enum: ACCEPT=0, DENY=1). No-op if the
/// slot is null.
fn continue_permission_callback(callback: *mut c_void, result: c_int) {
    let Some(cont) = read_fn(callback, CEF_PERMISSION_PROMPT_CALLBACK_CONT_OFFSET) else {
        return;
    };
    // SAFETY: read from `cef_permission_prompt_callback_t::cont`, whose pinned C
    // signature is `void (*)(cef_permission_prompt_callback_t*, int)`.
    let cont: unsafe extern "C" fn(*mut c_void, c_int) = unsafe { std::mem::transmute(cont) };
    // SAFETY: `callback` is the live CEF callback for this prompt, kept alive by our
    // stash ref until the paired `release_cef`.
    unsafe { cont(callback, result) };
}

/// Invoke `cef_media_access_callback_t::cont(self, allowed_permissions)`. CEF
/// expects the allowed media-access bitmask, not ACCEPT/DENY; denying is `0`.
/// No-op if the slot is null.
fn continue_media_access_callback(callback: *mut c_void, allowed_permissions: u32) {
    let Some(cont) = read_fn(callback, CEF_MEDIA_ACCESS_CALLBACK_CONT_OFFSET) else {
        return;
    };
    // SAFETY: read from `cef_media_access_callback_t::cont`, whose pinned C
    // signature is `void (*)(cef_media_access_callback_t*, uint32_t)`.
    let cont: unsafe extern "C" fn(*mut c_void, u32) = unsafe { std::mem::transmute(cont) };
    // SAFETY: `callback` is the live CEF callback for this prompt, kept alive by our
    // stash ref until the paired `release_cef`.
    unsafe { cont(callback, allowed_permissions) };
}

fn call_ref_counted_void(object: *mut c_void, offset: usize) {
    let Some(callback) = read_fn(object, offset) else {
        return;
    };
    // SAFETY: `callback` is read from a CEF ref-counted object function slot
    // using a header-verified offset.
    let callback: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(callback) };
    // SAFETY: caller passes a live CEF ref-counted object pointer.
    unsafe { callback(object) };
}

fn call_ref_counted_int(object: *mut c_void, offset: usize) {
    let Some(callback) = read_fn(object, offset) else {
        return;
    };
    // SAFETY: `callback` is read from a CEF ref-counted object function slot
    // using a header-verified offset.
    let callback: unsafe extern "C" fn(*mut c_void) -> c_int =
        unsafe { std::mem::transmute(callback) };
    // SAFETY: caller passes a live CEF ref-counted object pointer.
    let _ = unsafe { callback(object) };
}

fn read_fn(base: *mut c_void, offset: usize) -> Option<usize> {
    if base.is_null() {
        return None;
    }
    // SAFETY: caller supplies a live CEF struct pointer and a function-pointer
    // offset verified from the pinned CEF 149 headers.
    let value = unsafe { *((base.cast::<u8>()).add(offset).cast::<usize>()) };
    (value != 0).then_some(value)
}

#[cfg(test)]
mod tests;

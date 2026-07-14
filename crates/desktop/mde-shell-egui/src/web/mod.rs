//! The **Browser** surface — the sandboxed Servo browser rendered egui-native.
//!
//! BOOKMARKS-6 brokers the out-of-process `mde-web-preview` helper *into* the one
//! shell (the same EMBED model as the VDI Desktop surface): the helper renders
//! offscreen into a shared-memory frame; [`mde_web_preview_client`] receives that
//! frame fd over the per-session socket, maps it read-only, and hands the shell an
//! [`egui::ColorImage`]. This panel uploads that image to a `TextureHandle` on a
//! paint-ready (never a per-frame re-upload), paints it as the body, wires the
//! navigation chrome (back / forward / reload / address bar, §4 tokens) to the
//! control socket, and forwards this frame's egui input scaled by
//! `pixels_per_point`.
//!
//! ```text
//!   session.take_frame() ─▶ ColorImage ─▶ TextureHandle ─▶ ui paints the body
//!   chrome + ui.input     ───────────────────────────────▶ session control/input
//! ```
//!
//! Each tab is an independent [`WebSession`], so one page crashing surfaces an
//! honest "page crashed" state for THAT tab only (respawn-on-reload) and never
//! touches the others (per-session isolation). Spawning the live Servo helper is
//! the client crate's `live-helper` path, honest-gated to a GPU seat; with no live
//! session attached this surface shows an honest gated `EmptyState`, never a fake
//! page (§7).

use base64::Engine as _;
use mackes_mesh_types::peers::default_workgroup_root;
use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;
use mde_chat::{MessageKind, Severity};
use mde_editor_egui::spell::{self, SpellMiss};
use mde_egui::egui::{self, RichText, Sense, TextureHandle, TextureOptions};
use mde_egui::{muted_note, ChipTone, Style};
use mde_files_egui::transfers::{
    FileTransfers, Method as TransferMethod, TransferJob, TransferPolicy, TransferState,
    TransferVerb, TransfersClient,
};

use mde_web_preview_client::{
    confusable_reason, host_of, CertError, ConfusableReason, EditCommand, FilterListSource,
    FilterListStore, RequestFilter, SafeBrowsingBlocklist, SessionState, WebSession,
};
use qrcode::QrCode;
use std::collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

// ── live-helper: spawning the real sandboxed `mde-web-preview` helper ──────────
//
// Gated behind `mde-shell-egui`'s `live-helper` feature, which turns on the client
// crate's `live-helper` spawn API ([`WebSession::spawn`] + [`SpawnSpec`]). OFF by
// default so the shell stays portable and the Browser surface shows its honest
// gated EmptyState (§7); ON, the surface spawns the sandboxed helper on first open.
#[cfg(feature = "live-helper")]
use mde_web_preview_client::session::SpawnSpec;

/// The sandboxed Servo helper binary the RPM installs; overridable via
/// [`SERVO_HELPER_BIN_ENV`] for the test bed / dev builds.
#[cfg(feature = "live-helper")]
const DEFAULT_SERVO_HELPER_BIN: &str = "/usr/bin/mde-web-preview";

/// The env var overriding [`DEFAULT_SERVO_HELPER_BIN`] (test bed / dev builds).
#[cfg(feature = "live-helper")]
const SERVO_HELPER_BIN_ENV: &str = "MDE_WEB_PREVIEW_BIN";

/// The Chromium/CEF helper binary once BROWSER-DD-1 is packaged.
#[cfg(feature = "live-helper")]
const DEFAULT_CEF_HELPER_BIN: &str = "/usr/bin/mde-web-cef";

/// The env var overriding [`DEFAULT_CEF_HELPER_BIN`] (test bed / dev builds).
#[cfg(feature = "live-helper")]
const CEF_HELPER_BIN_ENV: &str = "MDE_WEB_CEF_BIN";

const CEF_DEVTOOLS_URL: &str = "http://127.0.0.1:9222/";
const CEF_DEVTOOLS_LIST_URL: &str = "http://127.0.0.1:9222/json/list";
const CEF_DEVTOOLS_TIMEOUT: Duration = Duration::from_millis(450);

/// Environment variable pointing at a pinned CEF bundle root (mirrors
/// `mde-web-cef`; duplicated here so the shell can gate honestly without
/// depending on the excluded helper crate).
#[cfg(feature = "live-helper")]
const CEF_ROOT_ENV: &str = "MDE_CEF_ROOT";

/// Conventional farm/vendor path for the pinned CEF bundle.
#[cfg(feature = "live-helper")]
const DEFAULT_CEF_ROOT: &str = "/opt/mde/cef";

/// The runtime library expected under the CEF bundle.
#[cfg(feature = "live-helper")]
const CEF_LIB_NAME: &str = "libcef.so";

/// CEF binary distributions install their runtime library under `Release/`.
#[cfg(feature = "live-helper")]
const CEF_RELEASE_DIR: &str = "Release";

/// CEF binary distributions install pak/ICU resources under `Resources/`.
#[cfg(feature = "live-helper")]
const CEF_RESOURCES_DIR: &str = "Resources";

#[cfg(feature = "live-helper")]
const CEF_ICU_DATA: &str = "icudtl.dat";

#[cfg(feature = "live-helper")]
const CEF_RESOURCES_PAK: &str = "resources.pak";

/// The native new-tab URL. A real helper session still loads this, while the shell
/// overlays the Quasar dashboard chrome for it.
const NEW_TAB_URL: &str = "about:blank";

/// The first page a freshly spawned live tab loads.
#[cfg(feature = "live-helper")]
const START_URL: &str = NEW_TAB_URL;

/// Front-door privacy explainer shown on the new-tab dashboard — the browser is
/// private by design (no persistent profile: the sandbox has no writable `$HOME`),
/// so history and cookies never outlive the session.
const PRIVATE_MODE_EXPLAINER: &str =
    "\u{1F512} Private by default \u{2014} history and cookies clear when you close the browser";

/// The fallback helper view geometry (device px) when no live seat size is known
/// yet (hermetic tests, first frame before the seat is probed). A live spawn
/// pre-sizes to the seat instead — see [`WebState::note_seat_px`].
#[cfg(feature = "live-helper")]
const INIT_W: u32 = 1280;
#[cfg(feature = "live-helper")]
const INIT_H: u32 = 800;

/// A per-axis ceiling (device px) for the pre-sized frame channel and for any live
/// resize target (browser-1). The shm frame region is `w * h * 4` bytes, so this
/// bounds one tab's channel to ~64 MiB even on an oversized seat; 4096 covers 4K
/// UHD (3840×2160) at native 1:1, and a larger seat paints clamped — still
/// click-correct via [`map_pointer_to_frame`], just gently upscaled.
const MAX_CHANNEL_DIM: u32 = 4096;

const CHROME_FONT: f32 = 10.0;
const CHROME_BUTTON: f32 = 20.0;
const CHROME_TAB_H: f32 = 22.0;
const CHROME_TAB_W: f32 = 132.0;
/// The floor a horizontal tab pill shrinks to once the strip is crowded. Below
/// this the strip stops shrinking and scrolls horizontally instead of wrapping
/// onto a second row (the standard desktop-browser overflow behaviour).
const CHROME_TAB_MIN_W: f32 = 54.0;
/// The fixed, compact width of a pinned tab's pill (favicon only, no title, no ×) —
/// Chrome's pinned tabs collapse to an icon. Constant so pinned tabs never shrink
/// under the crowded-strip overflow the way unpinned pills do.
const CHROME_TAB_PINNED_W: f32 = 24.0;
const CHROME_TAB_CLOSE: f32 = 18.0;
const CHROME_NEW_TAB_W: f32 = 58.0;
const CHROME_OMNIBOX_H: f32 = 22.0;
const CHROME_GAP: f32 = 2.0;
const DEFAULT_DENIED_PERMISSIONS: &str = "location, camera, microphone, notifications, clipboard";
const PAGE_ZOOM_MIN: u16 = 50;
const PAGE_ZOOM_MAX: u16 = 200;
const PAGE_ZOOM_STEP: u16 = 10;
const CUPS_PRINT_TIMEOUT: Duration = Duration::from_secs(5);

mod engine_runtime;
use engine_runtime::*;

/// The browser body is scaled to fill the surface, so sample it linearly.
const BROWSER_TEX: TextureOptions = TextureOptions::LINEAR;

/// One browser tab: its driven session and the GPU texture its frames upload into.
/// A named, colored tab group (Chrome-style): every tab carrying its index renders
/// a colored strip and can be operated on as a set (close-group). Session-only.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TabGroup {
    name: String,
    color: egui::Color32,
}

/// A distinct group color, cycled by group index over a fixed accent palette so
/// successive groups are visually separable. Pure so the cycling is unit-tested.
fn tab_group_color(index: usize) -> egui::Color32 {
    const PALETTE: [egui::Color32; 5] = [
        Style::ACCENT,
        Style::ACCENT_COMMS,
        Style::ACCENT_WORKLOADS,
        Style::ACCENT_TERMINALS,
        Style::WARN,
    ];
    PALETTE[index % PALETTE.len()]
}

struct Tab {
    /// The IPC + shm session driving one sandboxed helper.
    session: WebSession,
    /// Engine that owns this helper session.
    engine: BrowserEngine,
    /// Named container identity for the tab. Helpers are already one session per
    /// tab; this records the user-facing isolation bucket in the chrome.
    container: ContainerProfile,
    /// Browser UX intent for where this tab should land once the compositor-side
    /// multi-display handoff is wired. This is per-tab chrome state, not a fake
    /// output move.
    display_target: DisplayTarget,
    /// Tab-group membership — an index into [`WebState::tab_groups`], or `None` when
    /// the tab is ungrouped. Grouped tabs render a colored strip.
    group: Option<usize>,
    /// Whether the tab is pinned. Pinned tabs cluster at the front of the strip
    /// (the [`WebState::sort_pinned_stable`] invariant), render compact (favicon
    /// only, no title, no inline close), and survive a stray click on the ×
    /// (Chrome's pinned-tab affordance). Session-only, like every other tab bit.
    pinned: bool,
    /// Per-tab audio mute state mirrored to the helper.
    muted: bool,
    /// Per-tab forced dark rendering state mirrored to the helper.
    force_dark: bool,
    /// Per-tab reader-mode state mirrored to the helper.
    reader_mode: bool,
    /// Per-tab built-in userscript-library state mirrored to the helper.
    user_scripts: bool,
    /// Per-tab page-visible User-Agent override mirrored to the helper.
    user_agent: UserAgentOverride,
    /// Per-tab page-visible device profile override mirrored to the helper.
    device_profile: DeviceProfile,
    /// Last operator/page activity seen by the shell for idle-suspend accounting.
    last_activity: Instant,
    /// Whether this inactive tab has been shell-suspended after the idle timeout.
    idle_suspended: bool,
    /// Whether the painted page canvas owns keyboard/text input. This is tracked
    /// per tab instead of relying only on egui response focus, which can be lost
    /// when chrome widgets rebuild between frames.
    page_focused: bool,
    /// The body texture — allocated on the first frame, then updated in place with
    /// [`TextureHandle::set`] on each subsequent paint-ready (egui reuses the
    /// allocation, so a live page is not a per-frame upload churn).
    texture: Option<TextureHandle>,
    /// Last helper frame retained on the CPU side for viewport capture. The GPU
    /// texture is not readable, so capture uses this exact pre-upload image.
    /// Held behind an `Arc` so retaining it costs a refcount bump, not a
    /// full-resolution pixel deep copy, on every decoded frame (the same `Arc`
    /// is handed to the texture upload — `egui::ImageData` stores `Arc<ColorImage>`).
    last_frame: Option<std::sync::Arc<egui::ColorImage>>,
    /// Debounces panel-size changes into a single settled `session.resize` so a
    /// drag-resize drives the helper's CSS viewport once, not every frame
    /// (browser-1).
    resizer: ViewportResizer,
    /// The decoded favicon texture cache for this tab, keyed to the fingerprint of
    /// the [`WebSession::favicon`] PNG bytes it was built from — a favicon is
    /// PNG-decoded once per distinct set of bytes, not every frame (§Q7 bound).
    /// `None` until the page reports its first favicon; `Some` with an inner
    /// `texture: None` records "these exact bytes failed to decode" so a
    /// malformed favicon isn't retried every frame either.
    favicon_cache: Option<FaviconCache>,
}

/// One tab's decoded-favicon cache slot. See [`Tab::favicon_cache`].
#[derive(Clone)]
struct FaviconCache {
    /// A cheap hash of the PNG bytes the cached texture was decoded from.
    fingerprint: u64,
    /// The uploaded texture, or `None` when those bytes failed to PNG-decode.
    texture: Option<TextureHandle>,
}

/// How long a new panel device size must hold steady before it is committed to the
/// helper as a `session.resize` — long enough that a drag-resize sends ONE settled
/// resize instead of one per frame, short enough to feel immediate.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);

/// Debounces browser-panel viewport-size changes (browser-1, item 2).
///
/// The helper's page CSS viewport should track the real panel, but re-sending a
/// resize every frame during a window drag would thrash the engine's relayout. So
/// this tracks the last size actually committed to the helper plus a *candidate*
/// that must hold steady for [`RESIZE_DEBOUNCE`] before it is committed. A size
/// that flickers back to the committed value cancels the pending change; a no-op
/// frame (size unchanged) never resizes.
#[derive(Debug, Clone, Default, PartialEq)]
struct ViewportResizer {
    /// The size last committed to the helper (`None` = nothing sent yet).
    sent: Option<(u32, u32)>,
    /// A pending candidate size and the instant it was first observed.
    pending: Option<((u32, u32), Instant)>,
}

impl ViewportResizer {
    /// Fold this frame's `target` device size at time `now`. Returns `Some(size)`
    /// exactly once — on the frame a *changed* size settles (held ≥ `debounce`) —
    /// and `None` otherwise (unchanged, or still settling).
    fn observe(
        &mut self,
        target: (u32, u32),
        now: Instant,
        debounce: Duration,
    ) -> Option<(u32, u32)> {
        if self.sent == Some(target) {
            // Already at this size — cancel any pending change back to it.
            self.pending = None;
            return None;
        }
        match self.pending {
            Some((sz, since)) if sz == target => {
                if now.duration_since(since) >= debounce {
                    self.sent = Some(target);
                    self.pending = None;
                    return Some(target);
                }
            }
            // First sighting of a new candidate (or the candidate just changed):
            // (re)start its debounce clock.
            _ => self.pending = Some((target, now)),
        }
        None
    }

    /// Whether a change is still settling — the panel should keep repainting so the
    /// debounce fires even with no further input.
    const fn is_settling(&self) -> bool {
        self.pending.is_some()
    }
}

mod device_profile;
use device_profile::*;

mod site_data;
use site_data::*;
mod history;
use history::*;

mod printing;
use printing::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SavedPdf {
    path: PathBuf,
    url: String,
    title: String,
}

/// A CEF-intercepted download whose filename tripped [`download_is_dangerous`],
/// parked pending the user's explicit Keep/Discard choice (the downloads
/// drawer's "this type of file can harm your device" banner). Only one is
/// parked at a time — a second dangerous interception before the first is
/// resolved simply replaces it, matching the single-slot `insecure_prompt` /
/// `pending_saved_pdfs`-style gates already used elsewhere in this surface.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingDangerousDownload {
    id: u64,
    url: String,
    filename: String,
}

/// One saved login in the SESSION-ONLY credential store. Keyed by origin host so a
/// revisit offers it for autofill. In-memory only — never persisted to disk (the
/// browser is private-by-default; the sandbox has no writable $HOME). The user adds
/// these explicitly; auto-capture-on-submit is a separate, operator-reviewed feature.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredLogin {
    /// Host the credential belongs to (e.g. `mail.example.com`).
    host: String,
    /// Username / email.
    username: String,
    /// Password (session RAM only).
    password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

const fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

const fn plural_u32(count: u32) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

const DOWNLOADS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SEND_TAB_POLL_INTERVAL: Duration = Duration::from_secs(1);
const VOICE_COMMAND_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SHARE_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TRANSLATION_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const OFFLINE_CACHE_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SPEECH_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PASSKEY_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PASSKEY_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SECURITY_UPDATE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SESSION_SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_TAB_SUSPEND_AFTER: Duration = Duration::from_secs(30 * 60);
const CURATED_USERSCRIPT_COUNT: usize = 100;

// ── BOOKMARKS-BAR: the daemon bookmark-store mirror + single-row chrome layout ──
/// The daemon-retained converged bookmark [`mde_bookmarks::Collection`] topic —
/// the SAME `state/bookmarks/collection` the mackesd bookmarks worker publishes
/// and the Surface::Bookmarks manager hydrates from. The Browser mirrors it into
/// its own bar row (§6 local mirror of a Bus topic, never a mackesd dep).
const STATE_BOOKMARKS_COLLECTION: &str = "state/bookmarks/collection";
/// The bookmark collection is a persisted+synced store, not per-frame chatter, so
/// the bar mirror re-reads on a relaxed cadence (an explicit user act adds one).
const BOOKMARKS_COLLECTION_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The fixed slot width of one bookmark button on the single-row bar. Fixed so the
/// overflow split ([`bookmark_bar_visible_count`]) is exact — no font measuring.
const BOOKMARK_BTN_W: f32 = 132.0;
/// The width reserved for the ">>" overflow menu button when the row can't hold
/// every bookmark.
const BOOKMARK_OVERFLOW_W: f32 = 26.0;
/// The elision budget for a bookmark button's title (fits inside [`BOOKMARK_BTN_W`]
/// at [`CHROME_FONT`]); the full title rides the hover tooltip.
const BOOKMARK_TITLE_CHARS: usize = 18;

/// One top-level bookmark projected onto the bar: just the display title and its
/// navigation target. Folded from the daemon [`mde_bookmarks::Collection`]'s
/// render-ordered roots ([`bookmark_bar_links_from`]); the browser never re-derives
/// the CRDT tree — it mirrors the converged leaves it means to click.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BookmarkBarLink {
    /// The display title (falls back to the URL when the stored title is blank so a
    /// bar button always shows something legible).
    title: String,
    /// The navigation target handed to `load_target` / `request_new_tab_with_url`.
    url: String,
}

/// Fold a converged [`mde_bookmarks::Collection`] into the bar's top-level bookmark
/// links, in the collection's own render order (`roots()` is order-key sorted).
/// Folders are omitted — the bar is a flat quick-launch strip of the top-level
/// bookmarks, the same subset a browser's bookmarks bar surfaces.
fn bookmark_bar_links_from(collection: &mde_bookmarks::Collection) -> Vec<BookmarkBarLink> {
    collection
        .roots()
        .into_iter()
        .filter_map(|item| match item {
            mde_bookmarks::Item::Bookmark(b) => {
                let title = if b.title.trim().is_empty() {
                    b.url.clone()
                } else {
                    b.title
                };
                Some(BookmarkBarLink { title, url: b.url })
            }
            mde_bookmarks::Item::Folder(_) => None,
        })
        .collect()
}

/// Every bookmark in the collection — top-level AND nested in any folder — as
/// `{title, url}`. Feeds BOTH the toolbar star's bookmarked-state membership (via
/// [`bookmarked_url_set`]) and the omnibox bookmark autocomplete. Walks the tree via
/// [`mde_bookmarks::Collection::children`]; runs once per converged collection
/// update, not per frame. A blank stored title falls back to the URL (as the bar does).
fn all_bookmarks(collection: &mde_bookmarks::Collection) -> Vec<BookmarkBarLink> {
    let mut out = Vec::new();
    let mut folders = vec![None]; // Option<Uuid> stack, seeded with the roots
    while let Some(parent) = folders.pop() {
        for item in collection.children(parent) {
            match item {
                mde_bookmarks::Item::Bookmark(b) => {
                    let title = if b.title.trim().is_empty() {
                        b.url.clone()
                    } else {
                        b.title
                    };
                    out.push(BookmarkBarLink { title, url: b.url });
                }
                mde_bookmarks::Item::Folder(f) => folders.push(Some(f.id)),
            }
        }
    }
    out
}

/// The normalized-URL membership set for the toolbar star, derived from
/// [`all_bookmarks`] (Chrome's star is filled if the page lives in ANY folder).
fn bookmarked_url_set(bookmarks: &[BookmarkBarLink]) -> std::collections::HashSet<String> {
    bookmarks
        .iter()
        .map(|b| bookmark_membership_key(&b.url).to_owned())
        .collect()
}

/// Bookmarks whose title or URL contains the (case-insensitive) draft, most-relevant
/// first (title-prefix > url-prefix > substring), capped. Powers omnibox bookmark
/// autocomplete — the highest-signal suggestion class, so it renders above history.
fn matching_bookmarks(index: &[BookmarkBarLink], draft: &str, cap: usize) -> Vec<BookmarkBarLink> {
    let q = draft.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let rank = |b: &BookmarkBarLink| -> u8 {
        let (t, u) = (b.title.to_lowercase(), b.url.to_lowercase());
        if t.starts_with(&q) {
            0
        } else if u.starts_with(&q) || u.contains(&format!("://{q}")) {
            1
        } else {
            2
        }
    };
    let mut hits: Vec<&BookmarkBarLink> = index
        .iter()
        .filter(|b| b.title.to_lowercase().contains(&q) || b.url.to_lowercase().contains(&q))
        .collect();
    hits.sort_by_key(|b| rank(b));
    hits.into_iter().take(cap).cloned().collect()
}

/// Normalize a URL for bookmarked-state membership so a trailing-slash-only
/// difference (`https://x.com` vs `https://x.com/`) still lights the star, matching
/// Chrome's host-root equivalence without a full URL parse.
fn bookmark_membership_key(url: &str) -> &str {
    url.strip_suffix('/').unwrap_or(url)
}

/// How many of `total` fixed-width bookmark buttons fit on the single bar row of
/// `available` width. When every button fits, the return is `total` (no overflow
/// slot). Otherwise one `overflow_w`-wide ">>" slot is reserved and the return is
/// how many buttons precede it (possibly zero on a very narrow bar). Pure so the
/// overflow split is unit-tested without a GPU.
fn bookmark_bar_visible_count(
    total: usize,
    available: f32,
    btn_w: f32,
    gap: f32,
    overflow_w: f32,
) -> usize {
    if total == 0 {
        return 0;
    }
    // Width if every button sits on the row: n buttons with (n-1) inter-button gaps.
    let all = total as f32 * btn_w + (total.saturating_sub(1)) as f32 * gap;
    if all <= available {
        return total;
    }
    // They don't all fit: reserve the overflow button (its own leading gap) and
    // pack as many leading buttons as the remaining width allows.
    let budget = available - overflow_w - gap;
    let mut count = 0usize;
    let mut used = 0.0f32;
    while count < total {
        let next = if count == 0 {
            btn_w
        } else {
            used + gap + btn_w
        };
        if next > budget {
            break;
        }
        used = next;
        count += 1;
    }
    // At least one bookmark always lands in the overflow menu here, so cap the
    // visible run at `total - 1` even if rounding would otherwise show them all.
    count.min(total.saturating_sub(1))
}

mod userscripts;
use userscripts::*;

const fn download_state_rank(state: TransferState) -> u8 {
    match state {
        TransferState::Running => 0,
        TransferState::Queued => 1,
        TransferState::Paused => 2,
        TransferState::Failed => 3,
        TransferState::Done => 4,
    }
}

/// Extensions Chrome-style download-danger checks flag as potentially harmful:
/// executables, installers, scripts, and the package formats every desktop OS
/// this shell targets would happily run on double-click.
const DANGEROUS_DOWNLOAD_EXTENSIONS: &[&str] = &[
    "exe", "scr", "bat", "cmd", "com", "pif", "msi", "msix", "vbs", "vbe", "js", "jse", "jar",
    "ps1", "wsf", "hta", "cpl", "dll", "lnk", "reg", "sh", "run", "deb", "rpm", "dmg", "pkg",
    "apk", "gadget",
];

/// Pure classifier: does `filename` look like it could harm the device if run?
/// Case-insensitive on the final extension — the one that actually decides how
/// the OS opens the file — plus the second-to-last segment, so a masquerading
/// double extension is caught from either side (`invoice.pdf.exe` *and*
/// `invoice.exe.pdf` both flag, not just the visible-name trick). Paint-free
/// and side-effect-free so it's directly unit-testable ([`submit_download_to_ledger`]
/// is the only caller that acts on it).
fn download_is_dangerous(filename: &str) -> bool {
    fn is_dangerous_ext(part: &str) -> bool {
        DANGEROUS_DOWNLOAD_EXTENSIONS.contains(&part.to_ascii_lowercase().as_str())
    }
    let mut parts: Vec<&str> = filename.trim().split('.').collect();
    // A leading empty segment is a dotfile's leading dot (`.bashrc`), not an
    // extension boundary.
    if parts.first() == Some(&"") {
        parts.remove(0);
    }
    if parts.len() < 2 {
        return false;
    }
    if is_dangerous_ext(parts[parts.len() - 1]) {
        return true;
    }
    parts.len() >= 3 && is_dangerous_ext(parts[parts.len() - 2])
}

/// The filename a download should be saved under: the engine's suggested name
/// if it gave one, else derived from the URL's last non-empty path segment
/// (query/fragment stripped FIRST — otherwise a signed link like
/// `…/file.zip?token=x` derives the query as the filename instead of
/// `file.zip`).
fn resolve_download_filename(url: &str, filename: &str) -> String {
    let name = filename.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    let path_only = url.split(['?', '#']).next().unwrap_or(url);
    path_only
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("download")
        .to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TabOpenIntent {
    NewForeground(BrowserEngine),
    NewForegroundUrl { engine: BrowserEngine, url: String },
}

/// One entry on the session-only reopen stack (Ctrl+Shift+T / History →
/// Reopen Closed Tab).
///
/// Deliberately in-memory only: the stack is never written to disk, never part
/// of the session-sync snapshot, and never published to the Bus — closing a tab
/// must actually retire its trace (the Q74/Q80 privacy locks,
/// `docs/THREAT_MODEL.md`). It lives and dies with this shell process.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosedTab {
    /// The engine-committed URL the reopen loads.
    url: String,
    /// Last page title, used by the History menu's reopen item label.
    title: String,
    /// Engine that owned the closed session — the reopen keeps it.
    engine: BrowserEngine,
}

/// Maximum retained reopenable closed tabs — a short, bounded stack (Chrome
/// keeps a similarly short recently-closed list).
const CLOSED_TAB_STACK_CAP: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum BrowserEngine {
    #[default]
    Servo,
    Cef,
}

impl BrowserEngine {
    const fn label(self) -> &'static str {
        match self {
            Self::Servo => "Servo",
            Self::Cef => "CEF",
        }
    }

    const fn wire(self) -> &'static str {
        match self {
            Self::Servo => "servo",
            Self::Cef => "cef",
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "servo" => Some(Self::Servo),
            "cef" => Some(Self::Cef),
            _ => None,
        }
    }
}

/// The Browser surface's state: the open tabs, the active one, and the address-bar
/// edit buffer.
pub(crate) struct WebState {
    /// The open browser tabs (each an isolated session). Empty until a session
    /// attaches — spawning the live helper is the gated `live-helper` path.
    tabs: Vec<Tab>,
    /// Index of the active tab in [`Self::tabs`].
    active: usize,
    /// Engine selected for future live-helper tabs.
    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "read by live-helper spawning and Browser tests")
    )]
    engine: BrowserEngine,
    /// The address-bar edit buffer for the active tab.
    address: String,
    /// Whether the omnibox TextEdit owned keyboard focus on the last painted
    /// frame. Tracked as chrome state (the same idiom as [`Tab::page_focused`])
    /// because the per-frame engine→address sync runs BEFORE the omnibox is
    /// rebuilt each frame — an in-progress operator edit is never clobbered by
    /// an engine redirect.
    omnibox_focused: bool,
    /// Whether ANY Browser chrome text field (omnibox, find bar, dashboard
    /// search) owned keyboard focus on the last painted frame — the guard that
    /// keeps the tab accelerators (Ctrl+T/W/Tab/1-9) from firing mid-edit.
    chrome_edit_focus: bool,
    /// The active tab's engine-committed URL as of the last per-frame sync, so
    /// [`Self::sync_address_on_engine_nav`] rewrites the address bar only on a
    /// real engine transition (redirect / page navigation), not every frame —
    /// a blurred-but-unsubmitted draft survives until the engine really moves.
    last_engine_url: Option<String>,
    /// Set when Reload is pressed on a *crashed* active tab — the shell (or a test)
    /// drains it and swaps in a fresh session (respawn-on-reload).
    respawn_requested: bool,
    /// Set by the visible tab strip's `+` button or the session-restore seam.
    /// Live-helper builds drain this by spawning helper tabs; portable builds
    /// surface the honest gate only.
    open_requested: VecDeque<TabOpenIntent>,
    /// Set when Browser chrome asks to open the rich Bookmarks manager surface.
    open_bookmarks_requested: bool,
    /// Most-recently-closed tabs (newest LAST), bounded by
    /// [`CLOSED_TAB_STACK_CAP`], feeding Ctrl+Shift+T / History → Reopen
    /// Closed Tab. Session-only by design — see [`ClosedTab`] (Q74/Q80).
    closed_tabs: Vec<ClosedTab>,
    /// BROWSER-DD-2 vertical-tabs preference. This is purely chrome layout: it
    /// reuses the same tab/session operations and never creates a second tab model.
    vertical_tabs: bool,
    /// Immersive/fullscreen mode (F11): the browser chrome (tab strip, nav bar,
    /// bookmarks bar, drawers) is hidden and only the page body renders. Session-only.
    fullscreen: bool,
    /// HTTPS-only prompt latch. Explicit `http://` navigations pause here until
    /// the operator upgrades to HTTPS, continues over HTTP, or cancels.
    insecure_prompt: Option<String>,
    /// Quasar new-tab dashboard search draft. This is chrome state, not page
    /// content; submitted searches load the mesh SearXNG URL into the active tab.
    dashboard_query: String,
    /// New-tab speed-dial shortcuts. These start with mesh-local defaults but are
    /// browser state so session sync can carry an operator's current dashboard.
    speed_dial: Vec<SpeedDialEntry>,
    /// Page find draft shown in the compact find bar.
    find_query: String,
    /// The query of the last find submitted — a repeat means "next/prev match"
    /// (native find's `find_next`), a change means a fresh search from the top.
    last_find_query: Option<String>,
    /// Whether the compact find bar is open.
    find_open: bool,
    /// Current page zoom percentage sent to the active helper.
    page_zoom_percent: u16,
    /// BROWSER-DD-2 live SearXNG suggestions for the omnibox. Suggestions are
    /// fetched off-thread from the mesh-local service; the UI only polls this
    /// small state object so typing never blocks a frame.
    suggestions: SuggestionState,
    /// BROWSER-DD-11 offline spellcheck worker. Page text is extracted by the
    /// helper, then Hunspell runs off the UI thread and reports an honest result.
    spellcheck: SpellcheckState,
    /// Latest Browser page-text spellcheck result visible in the spelling drawer.
    latest_spellcheck: Option<BrowserSpellcheckResult>,
    /// Next shell-minted page-text request id for spellcheck/TTS seams.
    next_page_text_request_id: u64,
    /// Page-text requests owned by Browser spellcheck, keyed by request id.
    pending_spell_requests: BTreeMap<u64, usize>,
    /// Page-text requests owned by Browser read-aloud, keyed by request id.
    pending_read_aloud_requests: BTreeMap<u64, ReadAloudRequest>,
    /// Page-text requests owned by Power-mode scrape exports, keyed by request id.
    pending_scrape_export_requests: BTreeMap<u64, ScrapeExportRequest>,
    /// Page-text requests owned by Browser translate-page, keyed by request id.
    pending_translate_requests: BTreeMap<u64, TranslateRequest>,
    /// Page-text requests owned by Browser offline-cache snapshots, keyed by
    /// request id.
    pending_offline_cache_requests: BTreeMap<u64, OfflineCacheRequest>,
    /// BROWSER-DD-3 native blocker policy. Starts with the bundled seed lists so
    /// tracker blocking is default-on offline; synced/custom sources and per-site
    /// toggles mutate this store, then every open tab receives a freshly compiled
    /// [`RequestFilter`].
    adfilter_store: FilterListStore,
    /// Mesh-hosted safe-browsing host blocklists. The worker/file-sync half can
    /// replace these hosts; the Browser compiles them into every live session.
    safe_browsing_hosts: Vec<String>,
    /// BROWSER-DD-3 per-site permission manager. The helpers enforce default-deny
    /// for sensitive prompts; the shell tracks sites the operator explicitly
    /// forgot so the menu has a real state transition without offering fake allow
    /// toggles the engines cannot honor yet.
    forgotten_permission_sites: Vec<String>,
    /// BROWSER-DD-8 prompted-device API trail. Helpers still enforce default-deny;
    /// this records the operator-facing prompt/deny decisions per first-party site
    /// and publishes a typed handoff for the later engine grant hook.
    site_permission_prompts: Vec<SitePermissionPrompt>,
    /// BROWSER-DD-3 per-site data manager. Tracks committed first-party hosts,
    /// live tab counts, last-seen timestamps, and explicit clear actions.
    site_data: SiteDataManager,
    /// Session-only in-memory browsing history (B3, Q74/Q80 — never persisted).
    history: HistoryStore,
    /// Whether the History drawer is open.
    history_open: bool,
    /// Shared Transfers client. Browser downloads are just `browser_download`
    /// rows in the daemon-owned ledger, so Files and Browser show one queue.
    transfers: Box<dyn TransfersClient>,
    /// Browser-originated transfers filtered from the shared ledger.
    download_jobs: Vec<TransferJob>,
    /// Browser transfer ids already announced to the mesh notification feed.
    notified_downloads: BTreeSet<String>,
    /// BROWSER-DD-8 power mode. When enabled, the Browser exposes the developer /
    /// media / scrape tool menu; disabled keeps the default clean browser chrome.
    power_mode: bool,
    /// Last `action/browser/session-sync` body published. Keeps unchanged frames
    /// and ledger refreshes from flooding the Bus while still making every state
    /// transition observable.
    last_session_sync_body: Option<String>,
    /// One-shot startup restore latch. The Browser reads the daemon-owned latest
    /// session-sync snapshot once, before the live-helper blank-tab fallback.
    startup_restore_attempted: bool,
    /// Candidate roots for daemon-persisted startup restore snapshots. Production
    /// probes the local durable root first, then the Syncthing-backed workgroup
    /// root; tests inject temp roots without touching operator state.
    session_restore_roots: Vec<PathBuf>,
    /// Last time the Browser scanned the daemon-owned send-tab outbox for concrete
    /// node-addressed records.
    incoming_send_tab_last_poll: Option<Instant>,
    /// Last `event/browser-voice-command/<node>` ULID applied by this shell.
    voice_command_result_cursor: Option<String>,
    /// Last time the Browser scanned voice-command transcript results.
    voice_command_result_last_poll: Option<Instant>,
    /// Latest daemon-owned read-aloud/TTS status for this node.
    latest_read_aloud_status: Option<BrowserReadAloudStatus>,
    /// Latest daemon-owned voice-command/STT status for this node.
    latest_voice_command_status: Option<BrowserVoiceCommandStatus>,
    /// Last time the Browser scanned retained speech-owner status topics.
    speech_status_last_poll: Option<Instant>,
    /// Latest daemon-owned passkey/WebAuthn ceremony status for this node.
    latest_passkey_status: Option<BrowserPasskeyStatus>,
    /// Last time the Browser scanned retained passkey-owner status.
    passkey_status_last_poll: Option<Instant>,
    /// Last `event/browser-passkeys/<node>` ULID applied by this shell.
    passkey_result_cursor: Option<String>,
    /// Last time the Browser scanned passkey completion events.
    passkey_result_last_poll: Option<Instant>,
    /// Helper page request ids waiting for daemon passkey completion, keyed by
    /// the bridge-minted `client_request_id`.
    pending_passkey_requests: BTreeMap<String, usize>,
    /// Last `event/browser-share/<node>` ULID applied by this shell.
    share_result_cursor: Option<String>,
    /// Last time the Browser scanned accepted share route events.
    share_result_last_poll: Option<Instant>,
    /// Latest accepted daemon QR-share route visible in the Browser drawer.
    latest_qr_share: Option<BrowserQrShareResult>,
    /// Last `event/browser-translate/<node>` ULID applied by this shell.
    translation_result_cursor: Option<String>,
    /// Last time the Browser scanned translation results.
    translation_result_last_poll: Option<Instant>,
    /// Latest private translation result visible in the Browser drawer.
    latest_translation: Option<BrowserTranslationResult>,
    /// Last `event/browser-offline-cache/<node>` ULID applied by this shell.
    offline_cache_result_cursor: Option<String>,
    /// Last time the Browser scanned offline-cache record results.
    offline_cache_result_last_poll: Option<Instant>,
    /// Latest private offline-cache record visible in the Browser drawer.
    latest_offline_cache: Option<BrowserOfflineCacheResult>,
    /// Private cache records keyed by exact and conservative canonical URL aliases
    /// for unavailable-page fallback rendering. Records come only from the daemon
    /// cache owner.
    offline_cache_by_url: BTreeMap<String, BrowserOfflineCacheResult>,
    /// Latest daemon-owned CEF runtime update posture for this node.
    latest_security_update: Option<BrowserSecurityUpdateStatus>,
    /// Last time the Browser scanned the retained CEF update status topic.
    security_update_last_poll: Option<Instant>,
    /// Whether the compact download manager drawer is visible.
    downloads_open: bool,
    /// Last time the browser refreshed its ledger view.
    downloads_last_poll: Option<Instant>,
    /// Last time the per-frame catch-all rebuilt + published the session
    /// snapshot. Genuine mutations publish immediately via their own
    /// `publish_session_snapshot()` calls; this gate only throttles the
    /// unconditional per-paint rebuild in `web_panel`.
    session_snapshot_last_poll: Option<Instant>,
    /// Last lifecycle dispatch failure, shown inline instead of being swallowed.
    download_notice: Option<String>,
    /// A CEF-intercepted download flagged by [`download_is_dangerous`], parked
    /// pending the user's Keep/Discard choice instead of being silently
    /// submitted to the ledger. `None` once resolved either way; a safe
    /// download never touches this field.
    pending_dangerous_download: Option<PendingDangerousDownload>,
    /// Ledger job ids the user dismissed from the downloads drawer ("Remove
    /// from list" / "Clear all"). The Transfers ledger job itself is
    /// untouched — this only hides it from the Browser's own view, which
    /// [`WebState::refresh_downloads`] rebuilds from the ledger every poll.
    dismissed_download_ids: BTreeSet<String>,
    /// The source URL behind each ledger-submitted download, keyed by ledger
    /// job id, so the drawer's "Copy link" can put the real download URL on
    /// the clipboard — the ledger job's own `source` field is the local
    /// `.download.json` manifest path, not the URL.
    download_source_urls: BTreeMap<String, String>,
    /// Last viewport-capture result, shown inline instead of being swallowed.
    capture_notice: Option<String>,
    /// Last successfully saved user PDF. CUPS spool PDFs are excluded; this feeds
    /// the CEF-backed built-in PDF viewer action and offline-cache PDF snapshots.
    last_saved_pdf: Option<SavedPdf>,
    /// Helper-produced save-PDF requests waiting for confirmation, keyed by path.
    pending_saved_pdfs: BTreeMap<String, SavedPdf>,
    /// Compact CUPS destination/options drawer for Browser print jobs.
    print_settings_open: bool,
    /// Locally discovered CUPS destinations from `lpstat -e` plus default marker.
    cups_printers: Vec<CupsPrinter>,
    /// Last CUPS destination discovery error, shown inside the print drawer.
    cups_notice: Option<String>,
    /// Print options applied when the helper-produced PDF is submitted to CUPS.
    cups_settings: CupsPrintSettings,
    /// PDFs waiting for helper confirmation before submission to CUPS via `lp`.
    pending_cups_prints: BTreeMap<String, CupsPrintRequest>,
    /// Bus root for Browser-owned platform handoff actions. Defaults to the node
    /// client data dir; tests inject a temp root so persisted actions are asserted
    /// without touching the operator's real bus.
    bus_root: Option<PathBuf>,
    /// BOOKMARKS-BAR — whether the horizontal bookmarks bar is shown below the nav
    /// chrome. A session-only chrome toggle (View → Show Bookmarks Bar), defaulting
    /// hidden like the other browser chrome toggles (find / downloads / vertical).
    bookmarks_bar_visible: bool,
    /// Live query text for the tab-search dropdown (Chrome's "Search tabs" ⌄). A
    /// session-only, in-memory UI field — cleared when a result is chosen.
    tab_search_query: String,
    /// The top-level bookmark links mirrored from `state/bookmarks/collection` — the
    /// buttons the bar renders. Rebuilt each poll from the converged daemon
    /// collection; empty until the first snapshot is seen.
    bookmark_bar_links: Vec<BookmarkBarLink>,
    /// Membership set of every bookmarked URL (all folders, normalized via
    /// [`bookmark_membership_key`]) so the toolbar star reflects bookmarked state.
    bookmarked_urls: std::collections::HashSet<String>,
    /// Every bookmark (all folders) as `{title, url}` for omnibox autocomplete
    /// ([`matching_bookmarks`]). Derived alongside [`Self::bookmarked_urls`].
    bookmark_index: Vec<BookmarkBarLink>,
    /// Configurable search engines with keyword shortcuts ([`keyword_search_target`]).
    search_engines: Vec<SearchEngine>,
    /// Named, colored tab groups; a tab's [`Tab::group`] indexes into this. Session-only.
    tab_groups: Vec<TabGroup>,
    /// User-authored CSS site styles (safe userscript slice — CSS only), folded into
    /// the injected userscript bundle. Session-only.
    user_site_styles: Vec<UserSiteStyle>,
    /// Session HSTS: hosts the user chose to upgrade to HTTPS — future plain-http
    /// navigations to them auto-upgrade silently instead of re-prompting. In-memory
    /// only (no persistence, per the operator's session-HSTS decision).
    hsts_hosts: std::collections::HashSet<String>,
    /// Session-only per-site permission grants — `(origin, kind)` pairs the user
    /// ALLOWED this session (kind: 0 geolocation, 1 notifications, 2 clipboard). A
    /// future same-origin-same-kind request auto-allows without re-prompting; a
    /// block is never remembered (Chrome re-prompts after a block). In-memory only,
    /// per the operator's session-only permission decision (browser-gated-features).
    granted_permissions: std::collections::HashSet<(String, u8)>,
    /// The session-only saved-login store (see [`StoredLogin`]). In-memory; cleared
    /// on shell exit; never persisted. Drives the omnibox 🔑 autofill affordance.
    session_logins: Vec<StoredLogin>,
    /// Draft inputs for the 🔑 menu's "save a login for this site" mini-form.
    login_user_draft: String,
    login_pass_draft: String,
    /// An auto-captured login awaiting the user's "Save password?" decision (a form
    /// submit the engine beaconed). `None` when nothing is pending.
    pending_login_save: Option<StoredLogin>,
    /// Whether the Site Styles editor drawer is open, and its two input drafts.
    site_styles_open: bool,
    site_style_host_draft: String,
    site_style_css_draft: String,
    /// Bus cursor for the bookmark-collection mirror, so each converged snapshot is
    /// folded once (the exact `list_since` cursor idiom the other pollers use).
    bookmarks_collection_cursor: Option<String>,
    /// Throttle for the relaxed bookmark-collection re-read.
    bookmarks_collection_last_poll: Option<Instant>,
    /// Throttle for the operator-curated safe-browsing blocklist re-read.
    safe_browsing_last_poll: Option<Instant>,
    /// Region-capture mode is armed; the next drag over the page image writes a
    /// cropped PNG from the retained helper frame.
    capture_region_mode: bool,
    /// Region-capture drag anchor in helper-frame pixel coordinates.
    capture_region_start: Option<egui::Pos2>,
    /// Region-capture current drag point in helper-frame pixel coordinates.
    capture_region_current: Option<egui::Pos2>,
    /// An honest gated notice shown in place of the `EmptyState` when a `live-helper`
    /// open couldn't proceed (no seat · helper binary absent · spawn failed). `None`
    /// = the default gated caption. Only ever set on the live path — a named reason,
    /// never a fake page (§7).
    #[cfg(feature = "live-helper")]
    gate_notice: Option<String>,
    /// One-shot latch so the real `Command::spawn` is attempted at most once per
    /// surface lifetime — a spawn *failure* must not respawn a process every frame.
    #[cfg(feature = "live-helper")]
    spawn_attempted: bool,
    /// The live seat's output size in device pixels, refreshed each frame from the
    /// egui context ([`Self::note_seat_px`]). A freshly spawned helper pre-sizes its
    /// frame channel to this — the ceiling of any panel-driven resize — so an
    /// enlarged paint never overflows the channel (browser-1, item 3). Defaults to
    /// the `(INIT_W, INIT_H)` fallback until a real seat is seen.
    #[cfg(feature = "live-helper")]
    seat_px: (u32, u32),
}

impl Default for WebState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            engine: preferred_default_engine(),
            address: String::new(),
            omnibox_focused: false,
            chrome_edit_focus: false,
            last_engine_url: None,
            respawn_requested: false,
            open_requested: VecDeque::new(),
            open_bookmarks_requested: false,
            closed_tabs: Vec::new(),
            vertical_tabs: false,
            fullscreen: false,
            insecure_prompt: None,
            dashboard_query: String::new(),
            speed_dial: default_speed_dial(),
            find_query: String::new(),
            last_find_query: None,
            find_open: false,
            page_zoom_percent: 100,
            suggestions: SuggestionState::default(),
            spellcheck: SpellcheckState::default(),
            latest_spellcheck: None,
            next_page_text_request_id: 1,
            pending_spell_requests: BTreeMap::new(),
            pending_read_aloud_requests: BTreeMap::new(),
            pending_scrape_export_requests: BTreeMap::new(),
            pending_translate_requests: BTreeMap::new(),
            pending_offline_cache_requests: BTreeMap::new(),
            adfilter_store: FilterListStore::with_bundled(),
            safe_browsing_hosts: Vec::new(),
            forgotten_permission_sites: Vec::new(),
            site_permission_prompts: Vec::new(),
            site_data: SiteDataManager::default(),
            history: HistoryStore::default(),
            history_open: false,
            transfers: Box::new(FileTransfers::from_env()),
            download_jobs: Vec::new(),
            notified_downloads: BTreeSet::new(),
            power_mode: false,
            last_session_sync_body: None,
            startup_restore_attempted: false,
            session_restore_roots: default_session_restore_roots(),
            incoming_send_tab_last_poll: None,
            voice_command_result_cursor: None,
            voice_command_result_last_poll: None,
            latest_read_aloud_status: None,
            latest_voice_command_status: None,
            speech_status_last_poll: None,
            latest_passkey_status: None,
            passkey_status_last_poll: None,
            passkey_result_cursor: None,
            passkey_result_last_poll: None,
            pending_passkey_requests: BTreeMap::new(),
            share_result_cursor: None,
            share_result_last_poll: None,
            latest_qr_share: None,
            translation_result_cursor: None,
            translation_result_last_poll: None,
            latest_translation: None,
            offline_cache_result_cursor: None,
            offline_cache_result_last_poll: None,
            latest_offline_cache: None,
            offline_cache_by_url: BTreeMap::new(),
            latest_security_update: None,
            security_update_last_poll: None,
            downloads_open: false,
            downloads_last_poll: None,
            session_snapshot_last_poll: None,
            download_notice: None,
            pending_dangerous_download: None,
            dismissed_download_ids: BTreeSet::new(),
            download_source_urls: BTreeMap::new(),
            capture_notice: None,
            last_saved_pdf: None,
            pending_saved_pdfs: BTreeMap::new(),
            print_settings_open: false,
            cups_printers: Vec::new(),
            cups_notice: None,
            cups_settings: CupsPrintSettings::default(),
            pending_cups_prints: BTreeMap::new(),
            bus_root: mde_bus::client_data_dir(),
            bookmarks_bar_visible: false,
            tab_search_query: String::new(),
            bookmark_bar_links: Vec::new(),
            bookmarked_urls: std::collections::HashSet::new(),
            bookmark_index: Vec::new(),
            search_engines: default_search_engines(),
            tab_groups: Vec::new(),
            user_site_styles: Vec::new(),
            hsts_hosts: std::collections::HashSet::new(),
            granted_permissions: std::collections::HashSet::new(),
            session_logins: Vec::new(),
            login_user_draft: String::new(),
            login_pass_draft: String::new(),
            pending_login_save: None,
            site_styles_open: false,
            site_style_host_draft: String::new(),
            site_style_css_draft: String::new(),
            bookmarks_collection_cursor: None,
            safe_browsing_last_poll: None,
            bookmarks_collection_last_poll: None,
            capture_region_mode: false,
            capture_region_start: None,
            capture_region_current: None,
            #[cfg(feature = "live-helper")]
            gate_notice: None,
            #[cfg(feature = "live-helper")]
            spawn_attempted: false,
            #[cfg(feature = "live-helper")]
            seat_px: (INIT_W, INIT_H),
        }
    }
}

impl WebState {
    /// The active tab, if any.
    fn active_tab(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// WIN7-4 — the open-tab count, the SAME `self.tabs` length
    /// [`browser_accessibility_summary`] already folds into its "Active tab X
    /// of N" string (no second read, §7). Backs the Start Menu Browser
    /// tile's live fact.
    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    #[cfg(test)]
    fn with_transfers(mut self, transfers: Box<dyn TransfersClient>) -> Self {
        self.transfers = transfers;
        self.refresh_downloads();
        self
    }

    #[cfg(test)]
    fn with_bus_root(mut self, bus_root: Option<PathBuf>) -> Self {
        self.bus_root = bus_root;
        self
    }

    #[cfg(test)]
    fn with_session_restore_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.session_restore_roots = roots;
        self.startup_restore_attempted = false;
        self
    }

    /// Refresh the Browser's view of daemon-owned transfer jobs, keeping only
    /// browser-originated rows and prioritizing active work over history.
    fn refresh_downloads(&mut self) {
        let mut jobs: Vec<TransferJob> = self
            .transfers
            .jobs()
            .into_iter()
            .filter(|job| {
                job.method == TransferMethod::BrowserDownload
                    && !self.dismissed_download_ids.contains(&job.id)
            })
            .collect();
        jobs.sort_by(|a, b| {
            download_state_rank(a.state)
                .cmp(&download_state_rank(b.state))
                .then_with(|| b.updated_ms.cmp(&a.updated_ms))
                .then_with(|| b.created_ms.cmp(&a.created_ms))
        });
        if self.downloads_last_poll.is_some() {
            for job in &jobs {
                self.publish_download_notification(job);
            }
        }
        self.download_jobs = jobs;
        self.downloads_last_poll = Some(Instant::now());
        self.publish_session_snapshot();
    }

    fn publish_session_snapshot(&mut self) {
        let body = browser_session_sync_body(self);
        if self.last_session_sync_body.as_deref() == Some(body.as_str()) {
            return;
        }
        publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_SESSION_SYNC, &body);
        self.last_session_sync_body = Some(body);
    }

    /// Rebuild + publish the session snapshot at a UI-safe cadence. Genuine
    /// mutations (new tab, navigation, download completion, …) still publish
    /// immediately through their own `publish_session_snapshot()` calls; this
    /// per-frame catch-all only needs to pick up async tab-poll changes, so it
    /// runs ~1×/s instead of every vblank to avoid rebuilding the full
    /// serde_json body (and reallocating the open-tab host Vec) on every paint.
    /// The first frame still publishes immediately (last_poll is None), and the
    /// string-compare dedup in `publish_session_snapshot` prevents redundant
    /// bus traffic.
    fn poll_session_snapshot(&mut self) {
        if self
            .session_snapshot_last_poll
            .is_some_and(|last| last.elapsed() < SESSION_SNAPSHOT_POLL_INTERVAL)
        {
            return;
        }
        self.update_site_data_from_tabs();
        self.record_history_from_active_tab();
        self.publish_session_snapshot();
        self.session_snapshot_last_poll = Some(Instant::now());
    }

    fn publish_download_notification(&mut self, job: &TransferJob) {
        let (severity, summary) = match job.state {
            TransferState::Done => (
                Severity::Info,
                format!("Browser download complete: {}", short_transfer_name(job)),
            ),
            TransferState::Failed => (
                Severity::Warning,
                format!("Browser download failed: {}", short_transfer_name(job)),
            ),
            _ => return,
        };
        if !self.notified_downloads.insert(job.id.clone()) {
            return;
        }
        let body = browser_notify_body(
            severity,
            &summary,
            Some(&format!("{} -> {}", job.source, job.dest)),
        );
        publish_to_bus(self.bus_root.as_deref(), EVENT_NOTIFY_BROWSER, &body);
    }

    /// Poll the transfer ledger at a UI-safe cadence. The client reads local files,
    /// but the Browser still avoids scanning it every paint.
    fn poll_downloads(&mut self) {
        if self
            .downloads_last_poll
            .is_some_and(|last| last.elapsed() < DOWNLOADS_POLL_INTERVAL)
        {
            return;
        }
        self.refresh_downloads();
    }

    fn mark_tab_active(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.last_activity = Instant::now();
            tab.idle_suspended = false;
        }
    }

    fn mark_active_tab_activity(&mut self) {
        self.mark_tab_active(self.active);
    }

    fn suspend_idle_tabs(&mut self, now: Instant) {
        let mut suspended = Vec::new();
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            if idx == self.active || tab.idle_suspended || tab.session.is_crashed() {
                continue;
            }
            if now.duration_since(tab.last_activity) < IDLE_TAB_SUSPEND_AFTER {
                continue;
            }
            tab.session.stop();
            tab.idle_suspended = true;
            suspended.push((
                idx,
                tab.engine,
                tab.session.nav().url.clone(),
                tab.session.title().to_owned(),
            ));
        }
        for (idx, engine, url, title) in suspended {
            let body = browser_tab_suspend_body(idx, engine, &url, &title, IDLE_TAB_SUSPEND_AFTER);
            publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_TAB_SUSPEND, &body);
        }
        self.publish_session_snapshot();
    }

    fn download_counts(&self) -> (usize, usize) {
        (
            self.download_jobs
                .iter()
                .filter(|job| job.state.is_active())
                .count(),
            self.download_jobs.len(),
        )
    }

    fn dispatch_download_verb(&mut self, verb: TransferVerb) {
        match self.transfers.dispatch(&verb) {
            Ok(()) => {
                self.download_notice = None;
                self.refresh_downloads();
            }
            Err(err) => self.download_notice = Some(err),
        }
    }

    /// Append a session as a new tab and make it active. The live helper-spawn open
    /// path (gated) and the tests both funnel through here; the default gated build
    /// opens no tabs and shows the honest `EmptyState`, so this is unused there.
    #[cfg(test)]
    pub(crate) fn push_session(&mut self, session: WebSession) {
        self.push_session_with_engine(session, self.engine);
    }

    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "used by live-helper spawning and Browser tests")
    )]
    fn push_session_with_engine(&mut self, session: WebSession, engine: BrowserEngine) {
        let mut session = session;
        let url = session.nav().url.clone();
        session.set_filter(self.compiled_request_filter_for_url(&url));
        self.tabs.push(Tab {
            session,
            engine,
            container: ContainerProfile::None,
            display_target: DisplayTarget::Current,
            group: None,
            pinned: false,
            muted: false,
            force_dark: false,
            reader_mode: false,
            user_scripts: false,
            user_agent: UserAgentOverride::Default,
            device_profile: DeviceProfile::Default,
            last_activity: Instant::now(),
            idle_suspended: false,
            page_focused: false,
            texture: None,
            last_frame: None,
            resizer: ViewportResizer::default(),
            favicon_cache: None,
        });
        self.active = self.tabs.len() - 1;
        self.publish_session_snapshot();
    }

    /// Request a foreground tab. The surface owns the visible affordance; the shell
    /// live-helper path owns the process spawn, so tests and portable builds can
    /// assert the intent without fabricating a helper.
    fn request_new_tab(&mut self, engine: BrowserEngine) {
        self.open_requested
            .push_back(TabOpenIntent::NewForeground(engine));
    }

    fn request_new_tab_with_url(&mut self, engine: BrowserEngine, url: String) {
        self.open_requested
            .push_back(TabOpenIntent::NewForegroundUrl { engine, url });
    }

    /// Retain a closing tab on the bounded, session-only reopen stack. Blank
    /// sessions (no committed URL yet) are skipped — there is nothing to
    /// restore. The stack never leaves this process (Q74/Q80): no persistence,
    /// no snapshot, no Bus publish.
    fn remember_closed_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let url = tab.session.nav().url.trim().to_owned();
        if url.is_empty() {
            return;
        }
        self.closed_tabs.push(ClosedTab {
            url,
            title: tab.session.title().trim().to_owned(),
            engine: tab.engine,
        });
        if self.closed_tabs.len() > CLOSED_TAB_STACK_CAP {
            self.closed_tabs.remove(0);
        }
    }

    /// Reopen the most recently closed tab (Ctrl+Shift+T / History → Reopen
    /// Closed Tab): pop the reopen stack and enqueue a foreground open of its
    /// URL on its original engine — the exact open seam the tab strip's `+`
    /// buttons use. A drained stack is a silent no-op.
    fn restore_closed_tab(&mut self) {
        if let Some(closed) = self.closed_tabs.pop() {
            self.request_new_tab_with_url(closed.engine, closed.url);
        }
    }

    /// Cycle to the next tab, wrapping past the end (Ctrl+Tab).
    fn select_next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.select_tab((self.active + 1) % self.tabs.len());
        }
    }

    /// Cycle to the previous tab, wrapping past the start (Ctrl+Shift+Tab).
    fn select_prev_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.select_tab((self.active + self.tabs.len() - 1) % self.tabs.len());
        }
    }

    fn toggle_power_mode(&mut self) {
        self.power_mode = !self.power_mode;
        self.publish_session_snapshot();
    }

    fn open_active_view_source(&mut self) {
        let Some((engine, url)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            if url.is_empty() || tab.session.is_crashed() {
                None
            } else {
                Some((tab.engine, url))
            }
        }) else {
            self.capture_notice = Some("View source unavailable: no live page".to_owned());
            return;
        };
        let source_url = if url.starts_with("view-source:") {
            url
        } else {
            format!("view-source:{url}")
        };
        self.request_new_tab_with_url(engine, source_url);
        self.capture_notice = Some("Power mode: opening page source".to_owned());
    }

    fn open_chromium_devtools(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.capture_notice =
                Some("Chromium DevTools unavailable: no live CEF page".to_owned());
            return;
        };
        if tab.engine != BrowserEngine::Cef || tab.session.is_crashed() {
            self.capture_notice = Some("Chromium DevTools requires a live CEF tab".to_owned());
            return;
        }
        let active_url = tab.session.nav().url.trim().to_owned();
        let (url, notice) = match chromium_devtools_frontend_for_active_url(&active_url) {
            Ok(Some(url)) => (
                url,
                "Power mode: opening Chromium DevTools for active page".to_owned(),
            ),
            Ok(None) => (
                CEF_DEVTOOLS_URL.to_owned(),
                "Power mode: opening Chromium DevTools target list".to_owned(),
            ),
            Err(err) => (
                CEF_DEVTOOLS_URL.to_owned(),
                format!("Power mode: opening Chromium DevTools target list ({err})"),
            ),
        };
        self.request_new_tab_with_url(BrowserEngine::Cef, url);
        self.capture_notice = Some(notice);
    }

    /// Apply a Browser session-sync snapshot from the future mesh sync owner by
    /// restoring shell-owned settings and enqueueing tab opens through the existing
    /// live-helper path. The active tab is queued last because each live open
    /// foregrounds the newly attached helper.
    fn restore_session_sync_snapshot(&mut self, body: &str) -> Result<usize, String> {
        let v: serde_json::Value =
            serde_json::from_str(body).map_err(|err| format!("session snapshot JSON: {err}"))?;
        if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_session_sync") {
            return Err("session snapshot has the wrong op".to_owned());
        }
        let settings = v.get("settings").unwrap_or(&serde_json::Value::Null);
        if let Some(engine) = settings
            .get("future_engine")
            .and_then(serde_json::Value::as_str)
            .and_then(BrowserEngine::from_wire)
        {
            self.engine = engine;
        }
        if let Some(vertical) = settings
            .get("vertical_tabs")
            .and_then(serde_json::Value::as_bool)
        {
            self.vertical_tabs = vertical;
        }
        if let Some(zoom) = settings
            .get("page_zoom_percent")
            .and_then(serde_json::Value::as_u64)
        {
            self.page_zoom_percent = u16::try_from(zoom)
                .unwrap_or(PAGE_ZOOM_MAX)
                .clamp(PAGE_ZOOM_MIN, PAGE_ZOOM_MAX);
        }
        if let Some(find_open) = settings
            .get("find_open")
            .and_then(serde_json::Value::as_bool)
        {
            self.find_open = find_open;
        }
        if let Some(downloads_open) = settings
            .get("downloads_open")
            .and_then(serde_json::Value::as_bool)
        {
            self.downloads_open = downloads_open;
        }
        if let Some(power_mode) = settings
            .get("power_mode")
            .and_then(serde_json::Value::as_bool)
        {
            self.power_mode = power_mode;
        }
        if let Some(speed_dial) = speed_dial_from_settings(settings) {
            self.speed_dial = speed_dial;
        }

        let active_index = v.get("active_index").and_then(serde_json::Value::as_u64);
        let tabs = v
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "session snapshot is missing tabs[]".to_owned())?;
        let mut restore_tabs = Vec::new();
        for (fallback_index, tab) in tabs.iter().enumerate() {
            let url = tab
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            if url.is_empty() {
                continue;
            }
            let engine = tab
                .get("engine")
                .and_then(serde_json::Value::as_str)
                .and_then(BrowserEngine::from_wire)
                .unwrap_or(self.engine);
            let index = tab
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(fallback_index as u64);
            restore_tabs.push((index, engine, url.to_owned()));
        }
        restore_tabs.sort_by_key(|(index, _, _)| *index);
        if let Some(active_index) = active_index {
            restore_tabs.sort_by_key(|(index, _, _)| (*index == active_index, *index));
        }
        self.open_requested.clear();
        let count = restore_tabs.len();
        for (_, engine, url) in restore_tabs {
            self.request_new_tab_with_url(engine, url);
        }
        Ok(count)
    }

    /// One-shot startup restore from the daemon-owned latest snapshot files. The
    /// helper-spawn path drains the resulting open queue, so restore and ordinary
    /// new-tab creation stay on the same code path.
    fn restore_startup_session_once(&mut self) -> Option<usize> {
        if self.startup_restore_attempted {
            return None;
        }
        self.startup_restore_attempted = true;
        let host = local_hostname();
        for root in self.session_restore_roots.clone() {
            let path = session_sync_latest_path(&root, &host);
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            match self.restore_session_sync_snapshot(&body) {
                Ok(0) => continue,
                Ok(count) => return Some(count),
                Err(_) => continue,
            }
        }
        None
    }

    fn drain_incoming_send_tabs(&mut self) -> usize {
        let host = local_hostname();
        let sanitized_host = sanitize_session_host(&host);
        let mut opened = 0;
        let mut seen = BTreeSet::new();
        for root in self.session_restore_roots.clone() {
            let inbox = send_tab_inbox_dir(&root, &host);
            for path in incoming_send_tab_files(&root, &host) {
                let key = path
                    .strip_prefix(&inbox)
                    .map(|rel| rel.to_string_lossy().to_string())
                    .unwrap_or_else(|_| path.to_string_lossy().to_string());
                if !seen.insert(key) {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                match browser_send_tab_open_intent(&body, &sanitized_host) {
                    Ok((engine, url)) => {
                        self.request_new_tab_with_url(engine, url);
                        let _ = std::fs::remove_file(&path);
                        opened += 1;
                    }
                    Err(_) => continue,
                }
            }
        }
        opened
    }

    fn poll_incoming_send_tabs(&mut self) {
        if self
            .incoming_send_tab_last_poll
            .is_some_and(|last| last.elapsed() < SEND_TAB_POLL_INTERVAL)
        {
            return;
        }
        self.incoming_send_tab_last_poll = Some(Instant::now());
        self.drain_incoming_send_tabs();
    }

    fn poll_voice_command_results(&mut self) {
        if self
            .voice_command_result_last_poll
            .is_some_and(|last| last.elapsed() < VOICE_COMMAND_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.voice_command_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_voice_command_result_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.voice_command_result_cursor.as_deref())
        else {
            return;
        };
        for msg in msgs {
            self.voice_command_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(result) = parse_voice_transcript_result(body) else {
                continue;
            };
            self.apply_voice_transcript_result(result);
        }
    }

    /// BOOKMARKS-BAR — mirror the daemon's converged bookmark [`mde_bookmarks::Collection`]
    /// from `state/bookmarks/collection` into the bar's top-level link row. The
    /// EXACT cursor-based `list_since` idiom the sibling pollers use: read every new
    /// retained snapshot since the last cursor, keep the LAST one (the topic is
    /// retained-latest), and fold it into flat bar links. A `state/*` mirror only —
    /// the mackesd bookmarks worker stays the single writer of the op-log (§6).
    fn poll_bookmarks_collection(&mut self) {
        if self
            .bookmarks_collection_last_poll
            .is_some_and(|last| last.elapsed() < BOOKMARKS_COLLECTION_POLL_INTERVAL)
        {
            return;
        }
        self.bookmarks_collection_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let Ok(msgs) = persist.list_since(
            STATE_BOOKMARKS_COLLECTION,
            self.bookmarks_collection_cursor.as_deref(),
        ) else {
            return;
        };
        for msg in msgs {
            self.bookmarks_collection_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            if let Ok(collection) = serde_json::from_str::<mde_bookmarks::Collection>(body) {
                self.bookmark_bar_links = bookmark_bar_links_from(&collection);
                let all = all_bookmarks(&collection);
                self.bookmarked_urls = bookmarked_url_set(&all);
                self.bookmark_index = all;
            }
        }
    }

    /// Toggle the bookmarks bar (View → Show/Hide Bookmarks Bar). Session-only, like
    /// the vertical-tabs and downloads chrome toggles.
    fn toggle_bookmarks_bar(&mut self) {
        self.bookmarks_bar_visible = !self.bookmarks_bar_visible;
    }

    fn poll_speech_statuses(&mut self) {
        if self
            .speech_status_last_poll
            .is_some_and(|last| last.elapsed() < SPEECH_STATUS_POLL_INTERVAL)
        {
            return;
        }
        self.speech_status_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let host = local_hostname();
        let read_topic = browser_read_aloud_status_topic(&host);
        if let Ok(msgs) = persist.list_since(&read_topic, None) {
            for msg in msgs {
                let Some(body) = msg.body.as_deref() else {
                    continue;
                };
                if let Ok(status) = parse_read_aloud_status(body) {
                    self.latest_read_aloud_status = Some(status);
                }
            }
        }
        let voice_topic = browser_voice_command_status_topic(&host);
        if let Ok(msgs) = persist.list_since(&voice_topic, None) {
            for msg in msgs {
                let Some(body) = msg.body.as_deref() else {
                    continue;
                };
                if let Ok(status) = parse_voice_command_status(body) {
                    self.latest_voice_command_status = Some(status);
                }
            }
        }
    }

    fn poll_passkey_status(&mut self) {
        if self
            .passkey_status_last_poll
            .is_some_and(|last| last.elapsed() < PASSKEY_STATUS_POLL_INTERVAL)
        {
            return;
        }
        self.passkey_status_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_passkey_status_topic(&local_hostname());
        let Ok(mut msgs) = persist.list_since(&topic, None) else {
            return;
        };
        let Some(msg) = msgs.pop() else {
            return;
        };
        let Some(body) = msg.body.as_deref() else {
            return;
        };
        let Ok(status) = parse_passkey_status(body) else {
            return;
        };
        self.latest_passkey_status = Some(status);
    }

    fn poll_passkey_results(&mut self) {
        if self
            .passkey_result_last_poll
            .is_some_and(|last| last.elapsed() < PASSKEY_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.passkey_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_passkey_event_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.passkey_result_cursor.as_deref()) else {
            return;
        };
        for msg in msgs {
            self.passkey_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(completion) = parse_passkey_completion(body) else {
                continue;
            };
            let Some(tab_index) = self
                .pending_passkey_requests
                .remove(&completion.client_request_id)
            else {
                continue;
            };
            let Some(tab) = self.tabs.get_mut(tab_index) else {
                continue;
            };
            tab.session.complete_passkey(completion.body);
            self.capture_notice = Some("Passkey: returned result to page".to_owned());
        }
    }

    fn poll_share_results(&mut self) {
        if self
            .share_result_last_poll
            .is_some_and(|last| last.elapsed() < SHARE_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.share_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_share_result_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.share_result_cursor.as_deref()) else {
            return;
        };
        for msg in msgs {
            self.share_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(route) = parse_share_route_result(body) else {
                continue;
            };
            self.apply_share_route_result(route);
        }
    }

    fn poll_translation_results(&mut self) {
        if self
            .translation_result_last_poll
            .is_some_and(|last| last.elapsed() < TRANSLATION_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.translation_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_translation_result_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.translation_result_cursor.as_deref()) else {
            return;
        };
        for msg in msgs {
            self.translation_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(result) = parse_translation_result(body) else {
                continue;
            };
            self.apply_translation_result(result);
        }
    }

    fn poll_security_update_status(&mut self) {
        if self
            .security_update_last_poll
            .is_some_and(|last| last.elapsed() < SECURITY_UPDATE_STATUS_POLL_INTERVAL)
        {
            return;
        }
        self.security_update_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_security_update_status_topic(&local_hostname());
        let Ok(mut msgs) = persist.list_since(&topic, None) else {
            return;
        };
        let Some(msg) = msgs.pop() else {
            return;
        };
        let Some(body) = msg.body.as_deref() else {
            return;
        };
        let Ok(status) = parse_security_update_status(body) else {
            return;
        };
        self.latest_security_update = Some(status);
    }

    fn apply_translation_result(&mut self, result: BrowserTranslationResult) {
        if result.host != local_hostname() {
            return;
        }
        let chars = result.translation.chars().count();
        self.capture_notice = Some(format!(
            "Translation ready: {} character{}",
            chars,
            plural(chars)
        ));
        self.latest_translation = Some(result);
    }

    fn apply_share_route_result(&mut self, route: BrowserShareRouteResult) {
        if route.host != local_hostname() || route.target != BrowserShareTarget::Qr {
            return;
        }
        match qr_share_result(route) {
            Ok(result) => {
                self.capture_notice = Some("QR share ready".to_owned());
                self.latest_qr_share = Some(result);
            }
            Err(err) => {
                self.capture_notice = Some(format!("QR share unavailable: {err}"));
            }
        }
    }

    fn apply_voice_transcript_result(&mut self, result: VoiceTranscriptResult) {
        if result.host != local_hostname() {
            return;
        }
        match result.mode {
            VoiceCommandMode::Dictation => self.apply_voice_dictation(result),
            VoiceCommandMode::Command => self.apply_voice_command(&result.transcript),
        }
    }

    fn apply_voice_dictation(&mut self, result: VoiceTranscriptResult) {
        let Some(tab) = self.tabs.get_mut(result.tab_index) else {
            self.capture_notice =
                Some("Dictation result ignored: tab is no longer open".to_owned());
            return;
        };
        if result.focus != "page" || !tab.page_focused {
            self.capture_notice =
                Some("Dictation result ready: focus the page before dictating".to_owned());
            return;
        }
        let event = egui::Event::Text(result.transcript.clone());
        tab.session.send_input(&event, 1.0);
        tab.last_activity = Instant::now();
        self.capture_notice = Some(format!(
            "Dictation inserted {} character{}",
            result.transcript.chars().count(),
            plural(result.transcript.chars().count())
        ));
    }

    fn apply_voice_command(&mut self, transcript: &str) {
        let Some(action) = voice_command_action(transcript) else {
            self.capture_notice = Some(format!(
                "Voice command not recognized: {}",
                ellipsize(transcript.trim(), 48)
            ));
            return;
        };
        match action {
            BrowserVoiceAction::NewTab => {
                self.request_new_tab(self.engine);
                self.capture_notice = Some("Voice command: new tab".to_owned());
            }
            BrowserVoiceAction::CloseTab => {
                self.close_tab(self.active);
                self.capture_notice = Some("Voice command: close tab".to_owned());
            }
            BrowserVoiceAction::Back => {
                if let Some(tab) = self.active_tab() {
                    tab.session.go_back();
                    self.mark_active_tab_activity();
                    self.capture_notice = Some("Voice command: back".to_owned());
                }
            }
            BrowserVoiceAction::Forward => {
                if let Some(tab) = self.active_tab() {
                    tab.session.go_forward();
                    self.mark_active_tab_activity();
                    self.capture_notice = Some("Voice command: forward".to_owned());
                }
            }
            BrowserVoiceAction::Reload => {
                let crashed = self
                    .tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.session.is_crashed());
                if crashed {
                    self.respawn_requested = true;
                } else if let Some(tab) = self.active_tab() {
                    tab.session.reload();
                    self.mark_active_tab_activity();
                }
                self.capture_notice = Some("Voice command: reload".to_owned());
            }
            BrowserVoiceAction::ReadAloud => {
                self.request_active_read_aloud();
            }
            BrowserVoiceAction::Find(query) => {
                self.find_open = true;
                self.find_query = query;
                self.submit_find(false);
                self.capture_notice = Some("Voice command: find".to_owned());
            }
        }
    }

    fn request_bookmarks_manager(&mut self) {
        self.open_bookmarks_requested = true;
    }

    pub(crate) fn take_bookmarks_manager_request(&mut self) -> bool {
        std::mem::take(&mut self.open_bookmarks_requested)
    }

    /// Drain a pending new-tab request.
    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "drained by the live-helper shell path and tests")
    )]
    fn take_open_request(&mut self) -> Option<TabOpenIntent> {
        self.open_requested.pop_front()
    }

    /// Select an existing tab. Out-of-range indices are ignored so stale UI events
    /// cannot panic after a close.
    fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.mark_tab_active(index);
            self.sync_address_from_active();
            self.publish_session_snapshot();
        }
    }

    /// Close a tab and keep a stable active index. The helper child is killed by
    /// `WebSession`'s `Drop`, so this is the real process-teardown path. The
    /// closing tab's URL is retained on the in-memory reopen stack first, so
    /// every close affordance (strip ×, context menu, middle-click, Ctrl+W,
    /// voice) feeds Ctrl+Shift+T through this one seam.
    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.remember_closed_tab(index);
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
            self.address.clear();
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
            self.sync_address_from_active();
        } else if index <= self.active {
            self.active = self.active.saturating_sub(1);
            self.sync_address_from_active();
        }
        self.publish_session_snapshot();
    }

    /// Move one tab to a new index while preserving which session is active.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let active_tab = self.active;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = if active_tab == from {
            to
        } else if from < active_tab && to >= active_tab {
            active_tab.saturating_sub(1)
        } else if from > active_tab && to <= active_tab {
            active_tab + 1
        } else {
            active_tab
        };
        // A drag that crossed the pinned/unpinned boundary snaps back to its
        // cluster, keeping pinned tabs at the front (Chrome's invariant).
        self.sort_pinned_stable();
        self.sync_address_from_active();
        self.publish_session_snapshot();
    }

    /// Pin or unpin the tab at `index`, then re-cluster so pinned tabs sit at the
    /// front of the strip. No-op when the flag already matches.
    fn set_tab_pinned(&mut self, index: usize, pinned: bool) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        if tab.pinned == pinned {
            return;
        }
        tab.pinned = pinned;
        self.sort_pinned_stable();
        self.sync_address_from_active();
        self.publish_session_snapshot();
    }

    /// Re-establish the pinned-first invariant: pinned tabs cluster at the front in
    /// their existing relative order, unpinned follow in theirs. A *stable*
    /// partition, so a same-cluster reorder is preserved while a cross-boundary
    /// drag snaps back. Tracks which session stays active across the permutation.
    /// Early-returns when the strip is already partitioned (the common case) so a
    /// plain in-cluster drag costs no allocation.
    fn sort_pinned_stable(&mut self) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let boundary = self.tabs.iter().take_while(|t| t.pinned).count();
        if self.tabs.iter().skip(boundary).all(|t| !t.pinned) {
            return; // already pinned-first
        }
        let active = self.active;
        let mut order: Vec<usize> = (0..n).collect();
        // Stable sort by `!pinned`: pinned (key `false`) before unpinned (`true`);
        // equal keys keep their ascending original order (Rust `sort_by_key` is stable).
        order.sort_by_key(|&i| !self.tabs[i].pinned);
        let new_active = order.iter().position(|&i| i == active).unwrap_or(0);
        let mut slots: Vec<Option<Tab>> = self.tabs.drain(..).map(Some).collect();
        self.tabs = order.iter().map(|&i| slots[i].take().unwrap()).collect();
        self.active = new_active;
    }

    /// Duplicate the tab at `index` into a fresh foreground tab on the same engine
    /// and URL (Chrome's "Duplicate tab"), through the exact open seam the `+`
    /// buttons use. A blank tab duplicates to a new blank tab.
    fn duplicate_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let engine = tab.engine;
        let url = tab.session.nav().url.trim().to_owned();
        if url.is_empty() {
            self.request_new_tab(engine);
        } else {
            self.request_new_tab_with_url(engine, url);
        }
    }

    /// Close every tab except `keep` — and except pinned tabs, which Chrome's
    /// "Close other tabs" always spares. Closes right-to-left so indices stay valid;
    /// closed non-blank tabs land on the reopen stack; `keep` stays active.
    fn close_other_tabs(&mut self, keep: usize) {
        if keep >= self.tabs.len() {
            return;
        }
        let mut keep = keep;
        for i in (0..self.tabs.len()).rev() {
            if i != keep && !self.tabs[i].pinned {
                self.close_tab(i);
                if i < keep {
                    keep -= 1; // a removal left of `keep` shifts it down one
                }
            }
        }
        self.select_tab(keep);
    }

    /// Close every non-pinned tab to the right of `from` (Chrome's "Close tabs to
    /// the right"). Right-to-left so indices stay valid; pinned tabs are spared.
    fn close_tabs_to_the_right(&mut self, from: usize) {
        if from >= self.tabs.len() {
            return;
        }
        for i in (from + 1..self.tabs.len()).rev() {
            if !self.tabs[i].pinned {
                self.close_tab(i);
            }
        }
    }

    /// Put the tab at `index` into a fresh tab group (Chrome's "Add tab to new
    /// group"), minting the group with a cycled color and a default name.
    fn new_group_from_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let group_index = self.tab_groups.len();
        self.tab_groups.push(TabGroup {
            name: format!("Group {}", group_index + 1),
            color: tab_group_color(group_index),
        });
        self.tabs[index].group = Some(group_index);
    }

    /// Remove the tab at `index` from its group (leaves the group itself; other tabs
    /// keep their membership since group indices must stay stable).
    fn ungroup_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.group = None;
        }
    }

    fn set_vertical_tabs(&mut self, enabled: bool) {
        self.vertical_tabs = enabled;
        self.publish_session_snapshot();
    }

    fn toggle_vertical_tabs(&mut self) {
        self.set_vertical_tabs(!self.vertical_tabs);
    }

    #[cfg(test)]
    #[cfg_attr(
        not(feature = "live-helper"),
        allow(dead_code, reason = "used by live-helper-only Browser tests")
    )]
    fn select_engine(&mut self, engine: BrowserEngine) {
        self.engine = engine;
    }

    fn submit_address(&mut self) {
        let crashed = self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.session.is_crashed());
        if self.tabs.is_empty() || crashed {
            return;
        }
        // A keyword shortcut ("img sunset") wins over the default URL/search router.
        let Some(url) = keyword_search_target(&self.address, &self.search_engines)
            .or_else(|| omnibox_target(&self.address))
        else {
            return;
        };
        self.suggestions.clear();
        self.address = url.clone();
        self.load_target(url);
    }

    fn load_target(&mut self, url: String) {
        if is_plain_http(&url) {
            // Session HSTS: a host the user already upgraded auto-upgrades silently
            // (the one-shot recursion re-enters with an https URL, so is_plain_http
            // is false and it falls through to the normal load).
            if host_of(&url).is_some_and(|h| self.hsts_hosts.contains(&h)) {
                self.load_target(https_upgrade(&url));
                return;
            }
            self.insecure_prompt = Some(url);
            return;
        }
        if let Some(protocol) = ExternalProtocol::from_url(&url) {
            self.insecure_prompt = None;
            self.publish_external_protocol(protocol, &url);
            return;
        }
        self.insecure_prompt = None;
        self.mark_active_tab_activity();
        if let Some(tab) = self.active_tab() {
            tab.session.load(url);
        }
    }

    /// BOOKMARKS-BAR — open a bar bookmark. A plain click navigates the active tab
    /// (`load_target`, syncing the omnibox like the toolbar Go button); a
    /// middle-click — or a click with no live tab to reuse — opens it in a new
    /// foreground tab on the preferred engine (`request_new_tab_with_url`), the same
    /// two open seams the tab strip and History reopen already use.
    fn open_bookmark(&mut self, url: String, new_tab: bool) {
        if url.trim().is_empty() {
            return;
        }
        if new_tab || self.tabs.is_empty() {
            self.request_new_tab_with_url(self.engine, url);
            return;
        }
        self.address = url.clone();
        self.load_target(url);
    }

    fn publish_external_protocol(&mut self, protocol: ExternalProtocol, url: &str) {
        match protocol {
            ExternalProtocol::Tel => {
                let body = voice_dial_body(url);
                publish_to_bus(self.bus_root.as_deref(), ACTION_VOICE_DIAL, &body);
            }
            ExternalProtocol::Mailto | ExternalProtocol::Magnet => {
                let body = browser_protocol_handoff_body(protocol, url);
                publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_PROTOCOL, &body);
            }
        }
        self.capture_notice = Some(format!(
            "Handed {} link to {}",
            protocol.scheme(),
            protocol.target_label()
        ));
    }

    fn submit_dashboard_search(&mut self) {
        let q = self.dashboard_query.trim();
        if q.is_empty() {
            return;
        }
        let url = keyword_search_target(q, &self.search_engines)
            .unwrap_or_else(|| format!("{DEFAULT_SEARCH_URL}?q={}", percent_encode_query(q)));
        self.address = url.clone();
        self.load_target(url);
    }

    fn open_mesh_service(&mut self, url: String) {
        self.address = url.clone();
        self.load_target(url);
    }

    fn continue_insecure_load(&mut self) {
        let Some(url) = self.insecure_prompt.take() else {
            return;
        };
        self.address = url.clone();
        self.mark_active_tab_activity();
        if let Some(tab) = self.active_tab() {
            tab.session.load(url);
        }
    }

    fn upgrade_insecure_load(&mut self) {
        let Some(url) = self.insecure_prompt.take() else {
            return;
        };
        // Remember this host for the session so we auto-upgrade it next time (HSTS).
        if let Some(host) = host_of(&url) {
            self.hsts_hosts.insert(host);
        }
        let upgraded = https_upgrade(&url);
        self.address = upgraded.clone();
        self.mark_active_tab_activity();
        if let Some(tab) = self.active_tab() {
            tab.session.load(upgraded);
        }
    }

    fn cancel_insecure_load(&mut self) {
        self.insecure_prompt = None;
    }

    /// Clear ALL browsing data in one front-door action (Privacy → Clear all
    /// browsing data): the session-only history, every download from the list, the
    /// reopen-closed-tab stack, and the active tab's session state — the clears that
    /// were previously scattered across three separate drawers/menus. Everything here
    /// is session-only by design (nothing was ever persisted — Q74/Q80), so this
    /// forgets in-memory state rather than wiping a disk profile.
    fn clear_all_browsing_data(&mut self) {
        self.history.clear();
        self.dismiss_all_downloads();
        self.closed_tabs.clear();
        // Site data: saved logins + per-site permission grants are browsing data too,
        // so "Clear All Browsing Data" forgets them (a granted site then re-prompts;
        // a saved login must be re-entered). HSTS is deliberately NOT cleared — it's a
        // security upgrade, and forgetting it would downgrade a site back to plain http.
        self.session_logins.clear();
        self.granted_permissions.clear();
        self.clear_active_session_data();
    }

    fn clear_active_session_data(&mut self) {
        let cleared_host = self.active_first_party();
        self.mark_active_tab_activity();
        self.insecure_prompt = None;
        self.dashboard_query.clear();
        self.find_query.clear();
        self.find_open = false;
        self.page_zoom_percent = 100;
        self.suggestions.clear();
        self.address = NEW_TAB_URL.to_owned();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.texture = None;
            tab.last_frame = None;
            tab.muted = false;
            tab.force_dark = false;
            tab.reader_mode = false;
            tab.user_scripts = false;
            tab.user_agent = UserAgentOverride::Default;
            tab.device_profile = DeviceProfile::Default;
            tab.session.load(NEW_TAB_URL);
            tab.session.set_zoom(self.page_zoom_percent);
            tab.session.clear_find();
            tab.session.set_audio_muted(false);
            tab.session.set_force_dark(false);
            tab.session.set_reader_mode(false);
            tab.session.set_user_scripts(false, "");
            tab.session.set_user_agent("");
            tab.session
                .set_device_profile(DeviceProfile::Default.wire(), 0, 0, 100, false);
        }
        if let Some(host) = cleared_host {
            self.site_data.mark_cleared(&host, unix_ms());
        }
    }

    fn active_tab_has_frame(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.last_frame.is_some() && !tab.session.is_crashed())
    }

    fn capture_active_viewport(&mut self) {
        match self.capture_active_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_full_page(&mut self) {
        match self.capture_active_full_page_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured full page", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_mhtml(&mut self) {
        match self.capture_active_mhtml_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured MHTML", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_annotated_viewport(&mut self) {
        match self.capture_active_annotated_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured annotated", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_callout_viewport(&mut self) {
        match self.capture_active_callout_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured callout", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_freehand_viewport(&mut self) {
        match self.capture_active_freehand_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured freehand", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn export_active_media_manifest(&mut self) {
        match self
            .export_active_media_manifest_to_dirs(browser_media_spool_dir(), browser_capture_dir())
        {
            Ok(id) => {
                self.capture_notice = Some(format!("Power mode: queued media manifest ({id})"));
                self.refresh_downloads();
            }
            Err(err) => {
                self.capture_notice = Some(format!("Media manifest failed: {err}"));
            }
        }
    }

    fn export_active_media_manifest_to_dirs(
        &mut self,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<String, String> {
        let Some((url, title, engine, resources)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            if url.is_empty() || tab.session.is_crashed() {
                None
            } else {
                Some((
                    url,
                    tab.session.title().to_owned(),
                    tab.engine,
                    tab.session.recent_resource_requests(),
                ))
            }
        }) else {
            return Err("no live page to sniff".to_owned());
        };
        let now = unix_ms();
        std::fs::create_dir_all(&spool_dir)
            .map_err(|err| format!("create media spool dir: {err}"))?;
        std::fs::create_dir_all(&dest_dir)
            .map_err(|err| format!("create media destination dir: {err}"))?;
        let body = active_page_media_manifest(&url, &title, engine, now, &resources)?;
        let path = spool_dir.join(media_manifest_filename_for(&url, &title, now));
        std::fs::write(&path, body)
            .map_err(|err| format!("write media manifest {}: {err}", path.display()))?;
        enqueue_browser_output(
            self.transfers.as_ref(),
            &path.to_string_lossy(),
            dest_dir.to_string_lossy().as_ref(),
        )
    }

    fn download_observed_media_assets(&mut self) {
        match self.download_observed_media_assets_to_dirs(
            browser_media_spool_dir(),
            browser_capture_dir(),
        ) {
            Ok(ids) => {
                self.capture_notice = Some(format!(
                    "Power mode: queued observed media downloads ({} assets)",
                    ids.len()
                ));
                self.refresh_downloads();
            }
            Err(err) => {
                self.capture_notice = Some(format!("Media download queue failed: {err}"));
            }
        }
    }

    fn download_observed_image_assets(&mut self) {
        match self.download_observed_image_assets_to_dirs(
            browser_media_spool_dir(),
            browser_capture_dir(),
        ) {
            Ok(ids) => {
                self.capture_notice = Some(format!(
                    "Power mode: queued observed image downloads ({} assets)",
                    ids.len()
                ));
                self.refresh_downloads();
            }
            Err(err) => {
                self.capture_notice = Some(format!("Image download queue failed: {err}"));
            }
        }
    }

    fn download_observed_media_assets_to_dirs(
        &mut self,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<Vec<String>, String> {
        self.download_observed_assets_to_dirs(MediaAssetSelection::All, spool_dir, dest_dir)
    }

    fn download_observed_image_assets_to_dirs(
        &mut self,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<Vec<String>, String> {
        self.download_observed_assets_to_dirs(MediaAssetSelection::Images, spool_dir, dest_dir)
    }

    fn download_observed_assets_to_dirs(
        &mut self,
        selection: MediaAssetSelection,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<Vec<String>, String> {
        let Some((url, title, engine, resources)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            if url.is_empty() || tab.session.is_crashed() {
                None
            } else {
                Some((
                    url,
                    tab.session.title().to_owned(),
                    tab.engine,
                    tab.session.recent_resource_requests(),
                ))
            }
        }) else {
            return Err("no live page to download from".to_owned());
        };
        let now = unix_ms();
        std::fs::create_dir_all(&spool_dir)
            .map_err(|err| format!("create media download spool dir: {err}"))?;
        std::fs::create_dir_all(&dest_dir)
            .map_err(|err| format!("create media download destination dir: {err}"))?;
        let requests = active_page_media_asset_requests_with_selection(
            &url, &title, engine, now, &resources, selection,
        )?;
        if requests.is_empty() {
            return Err(selection.empty_error().to_owned());
        }
        let mut sources = Vec::with_capacity(requests.len());
        for (index, body) in requests.into_iter().enumerate() {
            let request_url = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["asset_url"].as_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| url.clone());
            let path = spool_dir.join(media_asset_request_filename_for(
                &url,
                &title,
                &request_url,
                now,
                index + 1,
            ));
            std::fs::write(&path, body)
                .map_err(|err| format!("write media download request {}: {err}", path.display()))?;
            sources.push(path.to_string_lossy().to_string());
        }
        enqueue_browser_output_batch(
            self.transfers.as_ref(),
            &sources,
            dest_dir.to_string_lossy().as_ref(),
        )
    }

    /// B2 — a browser download was intercepted by the engine (which cancelled its
    /// own write). A filename [`download_is_dangerous`] flags is parked in
    /// [`Self::pending_dangerous_download`] instead of being silently handed to
    /// the ledger — the downloads drawer surfaces a Keep/Discard warning, and
    /// only Keep resumes this same path via [`Self::enqueue_download_to_ledger`].
    /// A safe filename submits immediately, unchanged from before.
    fn submit_download_to_ledger(&mut self, id: u64, url: &str, filename: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        let filename = resolve_download_filename(url, filename);
        if download_is_dangerous(&filename) {
            self.pending_dangerous_download = Some(PendingDangerousDownload {
                id,
                url: url.to_owned(),
                filename,
            });
            self.downloads_open = true;
            return;
        }
        self.enqueue_download_to_ledger(id, url, &filename);
    }

    /// The user confirmed **Keep** on a dangerous-download warning — proceed
    /// exactly as a safe download would.
    fn keep_pending_dangerous_download(&mut self) {
        if let Some(pending) = self.pending_dangerous_download.take() {
            self.enqueue_download_to_ledger(pending.id, &pending.url, &pending.filename);
        }
    }

    /// The user chose **Discard** on a dangerous-download warning — drop it
    /// with no ledger job ever created.
    fn discard_pending_dangerous_download(&mut self) {
        self.pending_dangerous_download = None;
    }

    /// Write the `.download.json` manifest and enqueue the mesh Transfers job
    /// for a download already cleared to proceed — the daemon's
    /// browser-download lane fetches `asset_url` into the mesh share (the
    /// downloads drawer already renders the resulting `browser_download`
    /// ledger row).
    fn enqueue_download_to_ledger(&mut self, id: u64, url: &str, filename: &str) {
        let spool = browser_media_spool_dir();
        let dest = browser_capture_dir();
        if std::fs::create_dir_all(&spool).is_err() || std::fs::create_dir_all(&dest).is_err() {
            self.capture_notice =
                Some("Download failed: could not prepare the transfer spool".into());
            return;
        }
        let body = serde_json::json!({
            "op": "browser_media_download_request",
            "asset_url": url,
            "suggested_filename": filename,
        })
        .to_string();
        let path = spool.join(format!("browser-download-{id}-{}.download.json", unix_ms()));
        if std::fs::write(&path, body).is_err() {
            self.capture_notice =
                Some("Download failed: could not write the transfer request".into());
            return;
        }
        match enqueue_browser_output(
            self.transfers.as_ref(),
            &path.to_string_lossy(),
            dest.to_string_lossy().as_ref(),
        ) {
            Ok(job_id) => {
                self.download_source_urls.insert(job_id, url.to_owned());
                self.downloads_open = true;
                self.refresh_downloads();
                self.capture_notice = Some(format!("Downloading {filename} to the mesh share"));
            }
            Err(err) => self.capture_notice = Some(format!("Download failed: {err}")),
        }
    }

    /// Hide one ledger job from the Browser's downloads view without touching
    /// the ledger job itself (the drawer's per-item "Remove from list").
    fn dismiss_download(&mut self, id: &str) {
        self.dismissed_download_ids.insert(id.to_owned());
        self.download_source_urls.remove(id);
        self.download_jobs.retain(|job| job.id != id);
    }

    /// Hide every job currently visible in the downloads drawer (the header's
    /// "Clear all"). New downloads after this point are unaffected.
    fn dismiss_all_downloads(&mut self) {
        for job in &self.download_jobs {
            self.dismissed_download_ids.insert(job.id.clone());
            self.download_source_urls.remove(&job.id);
        }
        self.download_jobs.clear();
    }

    fn record_capture_success(&mut self, label: &str, path: &Path) {
        let notice = format!("{label} {}", path.display());
        self.capture_notice = Some(notice.clone());
        let body = browser_notify_body(Severity::Info, &notice, Some(&path.to_string_lossy()));
        publish_to_bus(self.bus_root.as_deref(), EVENT_NOTIFY_BROWSER, &body);
    }

    fn capture_active_viewport_to_dir(&self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let bytes = encode_color_image_png(frame)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name = capture_filename_for(&tab.session.nav().url, tab.session.title(), unix_ms());
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_full_page_to_dir(&self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let bytes = encode_color_image_png(frame)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_full_page_filename_for(&tab.session.nav().url, tab.session.title(), unix_ms());
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_mhtml_to_dir(&self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let png = encode_color_image_png(frame)?;
        let mhtml =
            mhtml_capture_document(&tab.session.nav().url, tab.session.title(), now_ms, &png);
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name = capture_mhtml_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, mhtml)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_annotated_viewport_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let caption =
            capture_annotation_caption(&tab.session.nav().url, tab.session.title(), now_ms);
        let annotated = annotate_capture_image(frame, &caption)?;
        let bytes = encode_color_image_png(&annotated)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_annotated_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_callout_viewport_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let caption =
            capture_annotation_caption(&tab.session.nav().url, tab.session.title(), now_ms);
        let annotated = annotate_callout_capture_image(frame, &caption)?;
        let bytes = encode_color_image_png(&annotated)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_callout_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_freehand_viewport_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let caption =
            capture_annotation_caption(&tab.session.nav().url, tab.session.title(), now_ms);
        let annotated = annotate_freehand_capture_image(frame, &caption)?;
        let bytes = encode_color_image_png(&annotated)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_freehand_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn start_region_capture(&mut self) {
        if !self.active_tab_has_frame() {
            self.capture_notice = Some("Capture failed: no painted page".to_owned());
            return;
        }
        self.capture_region_mode = true;
        self.capture_region_start = None;
        self.capture_region_current = None;
        self.capture_notice = Some("Drag a page region to capture".to_owned());
    }

    fn cancel_region_capture(&mut self) {
        self.capture_region_mode = false;
        self.capture_region_start = None;
        self.capture_region_current = None;
    }

    fn capture_active_region_to_dir(
        &self,
        dir: impl AsRef<Path>,
        region: PixelRegion,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let cropped = crop_color_image(frame, region)?;
        let bytes = encode_color_image_png(&cropped)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_region_filename_for(&tab.session.nav().url, tab.session.title(), unix_ms());
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn print_active_page(&mut self) {
        match self.queue_active_page_cups_print_to_dir(browser_print_spool_dir()) {
            Ok(path) => {
                self.capture_notice = Some(format!("CUPS print queued {}", path.display()));
            }
            Err(err) => {
                self.capture_notice = Some(format!("Print failed: {err}"));
            }
        }
    }

    fn toggle_print_settings(&mut self) {
        self.print_settings_open = !self.print_settings_open;
        if self.print_settings_open {
            self.refresh_cups_printers();
        }
    }

    fn handle_pdf_event(&mut self, path: String, ok: bool) -> String {
        if let Some(request) = self.pending_cups_prints.remove(&path) {
            if !ok {
                return format!("CUPS print failed: PDF write failed {}", request.path);
            }
            return match submit_pdf_to_cups(
                Path::new(&request.path),
                &request.title,
                &request.settings,
            ) {
                Ok(job) => format!("CUPS print submitted {job}"),
                Err(err) => format!("CUPS print failed: {err}"),
            };
        }
        if ok {
            let saved = self.pending_saved_pdfs.remove(&path).unwrap_or_else(|| {
                let (url, title) = self
                    .tabs
                    .get(self.active)
                    .map(|tab| {
                        (
                            tab.session.nav().url.clone(),
                            tab.session.title().to_owned(),
                        )
                    })
                    .unwrap_or_default();
                SavedPdf {
                    path: PathBuf::from(&path),
                    url,
                    title,
                }
            });
            self.last_saved_pdf = Some(saved);
            format!("PDF saved {path}")
        } else {
            self.pending_saved_pdfs.remove(&path);
            format!("PDF failed {path}")
        }
    }

    fn open_last_saved_pdf(&mut self) {
        match self.last_saved_pdf_viewer_url() {
            Ok(url) => {
                self.capture_notice = Some("Opening PDF in CEF viewer".to_owned());
                self.request_new_tab_with_url(BrowserEngine::Cef, url);
            }
            Err(err) => {
                self.capture_notice = Some(format!("PDF viewer failed: {err}"));
            }
        }
    }

    fn last_saved_pdf_viewer_url(&self) -> Result<String, String> {
        let Some(saved) = &self.last_saved_pdf else {
            return Err("no saved PDF".to_owned());
        };
        let path = &saved.path;
        if !pdf_file_looks_readable(path) {
            return Err(format!("{} is not a readable PDF", path.display()));
        }
        file_url_for_path(path)
    }

    fn save_active_page_pdf(&mut self) {
        match self.save_active_page_pdf_to_dir(browser_pdf_dir()) {
            Ok(path) => {
                self.capture_notice = Some(format!("PDF requested {}", path.display()));
            }
            Err(err) => {
                self.capture_notice = Some(format!("PDF failed: {err}"));
            }
        }
    }

    fn save_active_page_pdf_to_dir(&mut self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        if !self.can_drive_page_tools() {
            return Err("no live page".to_owned());
        }
        let (url, title) = {
            let Some(tab) = self.tabs.get(self.active) else {
                return Err("no active tab".to_owned());
            };
            (
                tab.session.nav().url.clone(),
                tab.session.title().to_owned(),
            )
        };
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name = pdf_filename_for(&url, &title, unix_ms());
        let path = dir.join(name);
        let key = path.to_string_lossy().into_owned();
        self.pending_saved_pdfs.insert(
            key.clone(),
            SavedPdf {
                path: path.clone(),
                url,
                title,
            },
        );
        if let Some(tab) = self.active_tab() {
            tab.session.save_pdf(key);
        }
        Ok(path)
    }

    fn can_drive_page_tools(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| !tab.session.is_crashed())
    }

    fn set_page_zoom(&mut self, percent: u16) {
        if !self.can_drive_page_tools() {
            return;
        }
        let percent = percent.clamp(PAGE_ZOOM_MIN, PAGE_ZOOM_MAX);
        self.page_zoom_percent = percent;
        if let Some(tab) = self.active_tab() {
            tab.session.set_zoom(percent);
        }
        self.publish_session_snapshot();
    }

    fn zoom_in(&mut self) {
        let next = self
            .page_zoom_percent
            .saturating_add(PAGE_ZOOM_STEP)
            .min(PAGE_ZOOM_MAX);
        self.set_page_zoom(next);
    }

    fn zoom_out(&mut self) {
        let next = self
            .page_zoom_percent
            .saturating_sub(PAGE_ZOOM_STEP)
            .max(PAGE_ZOOM_MIN);
        self.set_page_zoom(next);
    }

    fn reset_zoom(&mut self) {
        self.set_page_zoom(100);
    }

    fn set_active_tab_muted(&mut self, muted: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.muted = muted;
            tab.session.set_audio_muted(muted);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_mute(&mut self) {
        let muted = self.tabs.get(self.active).is_some_and(|tab| tab.muted);
        self.set_active_tab_muted(!muted);
    }

    fn set_active_tab_force_dark(&mut self, enabled: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.force_dark = enabled;
            tab.session.set_force_dark(enabled);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_force_dark(&mut self) {
        let enabled = self.tabs.get(self.active).is_some_and(|tab| tab.force_dark);
        self.set_active_tab_force_dark(!enabled);
    }

    fn set_active_tab_reader_mode(&mut self, enabled: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.reader_mode = enabled;
            tab.session.set_reader_mode(enabled);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_reader_mode(&mut self) {
        let enabled = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.reader_mode);
        self.set_active_tab_reader_mode(!enabled);
    }

    /// Add the drafted host + CSS as a user site style (the safe, CSS-only userscript
    /// slice) and re-inject if userscripts are on. Blank drafts are ignored; a
    /// successful add clears the drafts.
    fn add_user_site_style(&mut self) {
        let host = self.site_style_host_draft.trim().to_owned();
        let css = self.site_style_css_draft.trim().to_owned();
        if host.is_empty() || css.is_empty() {
            return;
        }
        self.user_site_styles.push(UserSiteStyle { host, css });
        self.site_style_host_draft.clear();
        self.site_style_css_draft.clear();
        self.reinject_user_scripts_if_active();
    }

    fn remove_user_site_style(&mut self, index: usize) {
        if index < self.user_site_styles.len() {
            self.user_site_styles.remove(index);
            self.reinject_user_scripts_if_active();
        }
    }

    /// Re-push the userscript bundle to the active tab when its userscripts toggle is
    /// on, so a change to the user site styles takes effect immediately.
    fn reinject_user_scripts_if_active(&mut self) {
        if self.tabs.get(self.active).is_some_and(|t| t.user_scripts) {
            self.set_active_tab_user_scripts(true);
        }
    }

    fn set_active_tab_user_scripts(&mut self, enabled: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        let bundle = if enabled {
            curated_userscript_bundle(&self.user_site_styles)
        } else {
            String::new()
        };
        if let Some(tab) = self.active_tab() {
            tab.user_scripts = enabled;
            tab.session.set_user_scripts(enabled, bundle);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_user_scripts(&mut self) {
        let enabled = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.user_scripts);
        self.set_active_tab_user_scripts(!enabled);
    }

    fn set_active_tab_user_agent(&mut self, user_agent: UserAgentOverride) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.user_agent = user_agent;
            tab.session.set_user_agent(user_agent.value());
        }
        self.publish_session_snapshot();
    }

    fn cycle_active_tab_user_agent(&mut self) {
        let user_agent = self
            .tabs
            .get(self.active)
            .map_or(UserAgentOverride::Default, |tab| tab.user_agent)
            .next();
        self.set_active_tab_user_agent(user_agent);
    }

    fn set_active_tab_device_profile(&mut self, device_profile: DeviceProfile) {
        if !self.can_drive_page_tools() {
            return;
        }
        let (width, height, scale_percent, touch) = device_profile.dimensions();
        if let Some(tab) = self.active_tab() {
            tab.device_profile = device_profile;
            tab.session.set_device_profile(
                device_profile.wire(),
                width,
                height,
                scale_percent,
                touch,
            );
        }
        self.publish_session_snapshot();
    }

    fn cycle_active_tab_device_profile(&mut self) {
        let device_profile = self
            .tabs
            .get(self.active)
            .map_or(DeviceProfile::Default, |tab| tab.device_profile)
            .next();
        self.set_active_tab_device_profile(device_profile);
    }

    fn request_active_voice_command(&mut self, mode: VoiceCommandMode) {
        if !self.can_drive_page_tools() {
            self.capture_notice = Some(format!("{} unavailable: no live page", mode.label()));
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            self.capture_notice = Some(format!("{} unavailable: no live page", mode.label()));
            return;
        };
        let body = browser_voice_command_body(
            mode,
            self.active,
            tab.engine,
            &tab.session.nav().url,
            tab.session.title(),
            &self.address,
            tab.page_focused,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            ACTION_BROWSER_VOICE_COMMAND,
            &body,
        );
        self.capture_notice = Some(format!("{}: sent STT request", mode.label()));
    }

    fn handle_page_text_event(&mut self, id: u64, text: String) {
        if let Some(tab_index) = self.pending_spell_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Spelling: no page text found".to_owned());
                return;
            }
            self.capture_notice = Some("Spelling: checking page text".to_owned());
            self.spellcheck.start(id, tab_index, text);
            return;
        }
        if let Some(request) = self.pending_read_aloud_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Read aloud: no page text found".to_owned());
                return;
            }
            let body = browser_read_aloud_body(&request, &text);
            publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_READ_ALOUD, &body);
            self.capture_notice = Some("Read aloud: sent page text to TTS".to_owned());
            return;
        }
        if let Some(request) = self.pending_translate_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Translate: no page text found".to_owned());
                return;
            }
            let body = browser_translate_body(&request, &text);
            publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_TRANSLATE, &body);
            self.capture_notice = Some("Translate: sent page text to translation".to_owned());
            return;
        }
        if let Some(request) = self.pending_offline_cache_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Offline cache: no page text found".to_owned());
                return;
            }
            let body = browser_offline_cache_body(&request, &text);
            publish_to_bus(
                self.bus_root.as_deref(),
                ACTION_BROWSER_OFFLINE_CACHE,
                &body,
            );
            self.capture_notice = Some("Offline cache: saved page snapshot".to_owned());
        }
    }

    fn handle_passkey_event(&mut self, tab_index: usize, engine: BrowserEngine, body: &str) {
        match browser_passkey_body(engine, body) {
            Ok(handoff_body) => {
                let client_request_id = passkey_client_request_id(body);
                publish_to_bus(
                    self.bus_root.as_deref(),
                    ACTION_BROWSER_PASSKEY,
                    &handoff_body,
                );
                if let Some(client_request_id) = client_request_id {
                    self.pending_passkey_requests
                        .insert(client_request_id, tab_index);
                }
                self.capture_notice = Some("Passkey: sent ceremony to daemon".to_owned());
            }
            Err(err) => {
                self.capture_notice = Some(format!("Passkey: ignored helper event ({err})"));
            }
        }
    }

    fn set_active_tab_container(&mut self, container: ContainerProfile) {
        if let Some(tab) = self.active_tab() {
            tab.container = container;
        }
        self.publish_session_snapshot();
    }

    fn cycle_active_tab_container(&mut self) {
        let next = self
            .tabs
            .get(self.active)
            .map_or(ContainerProfile::None, |tab| tab.container.next());
        self.set_active_tab_container(next);
    }

    fn set_active_tab_display_target(&mut self, display_target: DisplayTarget) {
        let active = self.active;
        let body = if let Some(tab) = self.tabs.get_mut(active) {
            tab.display_target = display_target;
            Some(browser_display_target_body(active, tab, display_target))
        } else {
            None
        };
        if let Some(body) = body {
            publish_to_bus(
                self.bus_root.as_deref(),
                ACTION_BROWSER_DISPLAY_TARGET,
                &body,
            );
            self.publish_session_snapshot();
        }
    }

    fn cycle_active_tab_display_target(&mut self) {
        let next = self
            .tabs
            .get(self.active)
            .map_or(DisplayTarget::Current, |tab| tab.display_target.next());
        self.set_active_tab_display_target(next);
    }

    fn open_find_bar(&mut self) {
        if !self.can_drive_page_tools() {
            return;
        }
        self.find_open = true;
        self.publish_session_snapshot();
    }

    fn close_find_bar(&mut self) {
        self.find_open = false;
        self.last_find_query = None;
        if let Some(tab) = self.active_tab() {
            tab.session.clear_find();
        }
        self.publish_session_snapshot();
    }

    /// Apply an in-page context-menu action to the tab at `index`.
    fn apply_page_context_action(&mut self, index: usize, action: PageContextAction) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        match action {
            PageContextAction::Back => tab.session.go_back(),
            PageContextAction::Forward => tab.session.go_forward(),
            PageContextAction::Reload => tab.session.reload(),
            PageContextAction::Edit(command) => tab.session.edit_command(command),
        }
    }

    fn submit_find(&mut self, backwards: bool) {
        let query = self.find_query.trim().to_owned();
        if query.is_empty() {
            self.last_find_query = None;
            if let Some(tab) = self.active_tab() {
                tab.session.clear_find();
            }
            return;
        }
        // Same query as last time → cycle to the next/prev match; a changed query
        // starts a fresh search from the top.
        let find_next = self.last_find_query.as_deref() == Some(query.as_str());
        self.last_find_query = Some(query.clone());
        if let Some(tab) = self.active_tab() {
            tab.session.find_in_page(query, backwards, find_next);
        }
    }

    /// The active tab's find tally `(active_ordinal, total_count)` for the counter.
    fn active_find_result(&self) -> Option<(u32, u32)> {
        self.tabs.get(self.active)?.session.find_result()
    }

    fn sync_address_from_active(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            let url = tab.session.nav().url.trim();
            if !url.is_empty() {
                self.address = url.to_owned();
            }
        }
    }

    /// Per-frame omnibox ↔ engine sync: an engine-driven navigation (redirect,
    /// page script, in-page link click) updates the address bar even though no
    /// chrome action (tab select/close/move) ran. Guarded two ways so it can
    /// run every frame from the pump: it only rewrites the address when the
    /// active tab's engine URL actually CHANGED since the last frame, and never
    /// while the omnibox itself owns keyboard focus — so it cannot clobber an
    /// in-progress operator edit, and a blurred-but-unsubmitted draft survives
    /// until the engine really moves.
    fn sync_address_on_engine_nav(&mut self) {
        let Some(url) = self
            .tabs
            .get(self.active)
            .map(|tab| tab.session.nav().url.trim().to_owned())
        else {
            self.last_engine_url = None;
            return;
        };
        if self.last_engine_url.as_deref() == Some(url.as_str()) {
            return;
        }
        if !self.omnibox_focused && !url.is_empty() {
            self.address.clone_from(&url);
        }
        // Fold the transition even when focus suppressed the rewrite: lifting
        // focus later must not retroactively apply a stale engine URL over
        // whatever the operator left in the bar.
        self.last_engine_url = Some(url);
    }

    fn poll_suggestions(&mut self) {
        self.suggestions.poll();
    }

    fn update_suggestions_for_address(&mut self) {
        self.suggestions.update_for_draft(&self.address);
        // History matches independently of the SearXNG fetch gate
        // (`should_fetch_suggestions`): a URL-like draft that skips the search
        // round-trip should still surface a matching visit. Guarded on a
        // non-empty trimmed draft so an empty omnibox doesn't dump the whole
        // recent-visits list into the dropdown.
        let hits: Vec<String> = if self.address.trim().is_empty() {
            Vec::new()
        } else {
            self.history
                .matching(&self.address)
                .map(|v| v.url.clone())
                .take(5)
                .collect()
        };
        self.suggestions.set_history_matches(hits);
        // Bookmark matches (title OR url) — highest-signal, rendered above history.
        let bookmarks = matching_bookmarks(&self.bookmark_index, &self.address, 3);
        self.suggestions.set_bookmark_matches(bookmarks);
        // Inline top-hit: preselect the first suggestion when it is an inline
        // completion of the draft (Chrome's omnibox), so Enter accepts the completed
        // URL; otherwise nothing is preselected and arrow keys drive the highlight.
        let ordered = self.suggestions.ordered_commit_values();
        self.suggestions.selected = inline_top_hit(&ordered, &self.address);
    }

    fn accept_suggestion(&mut self, suggestion: String) {
        self.address = suggestion;
        self.submit_address();
    }

    /// Whether a crashed tab's Reload asked for a respawn — drained by the shell
    /// each frame (and by the tests). The live build swaps in a fresh session via
    /// [`Self::respawn_active_with`]; the gated build acknowledges it honestly.
    pub(crate) fn take_respawn_request(&mut self) -> bool {
        std::mem::take(&mut self.respawn_requested)
    }

    /// Replace the active tab's crashed session with a fresh one (respawn-on-reload),
    /// discarding its stale texture so the new page uploads cleanly.
    #[cfg(test)]
    pub(crate) fn respawn_active_with(&mut self, session: WebSession) {
        let engine = self
            .tabs
            .get(self.active)
            .map_or(self.engine, |tab| tab.engine);
        self.respawn_active_with_engine(session, engine);
    }

    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "used by live-helper respawn and Browser tests")
    )]
    fn respawn_active_with_engine(&mut self, session: WebSession, engine: BrowserEngine) {
        let mut session = session;
        let url = session.nav().url.clone();
        session.set_filter(self.compiled_request_filter_for_url(&url));
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.session = session;
            tab.engine = engine;
            tab.texture = None;
            tab.last_frame = None;
            tab.last_activity = Instant::now();
            tab.idle_suspended = false;
            // A fresh helper re-negotiates its viewport from scratch.
            tab.resizer = ViewportResizer::default();
        }
        self.publish_session_snapshot();
    }

    fn compiled_request_filter(&self) -> RequestFilter {
        RequestFilter::from_store(&self.adfilter_store)
            .with_safe_browsing(SafeBrowsingBlocklist::from_hosts(&self.safe_browsing_hosts))
    }

    /// Record a session-only permission grant `(origin, kind)`.
    fn grant_permission(&mut self, origin: &str, kind: u8) {
        self.granted_permissions.insert((origin.to_owned(), kind));
    }

    /// Whether `(origin, kind)` was allowed earlier this session.
    fn is_permission_granted(&self, origin: &str, kind: u8) -> bool {
        self.granted_permissions
            .contains(&(origin.to_owned(), kind))
    }

    /// Resolve the active tab's pending permission request, if any: a capability
    /// this origin was already granted this session auto-allows (answers the engine
    /// with `true`, no prompt) and returns `None`; otherwise returns `(origin, kind)`
    /// for the shell to render a prompt. Never auto-denies — a previously-blocked
    /// capability re-prompts (Chrome's behaviour).
    fn pending_permission_prompt(&mut self) -> Option<(String, u8)> {
        let (origin, kind) = self
            .tabs
            .get(self.active)?
            .session
            .pending_permission()
            .map(|req| (req.origin.clone(), req.kind))?;
        if self.is_permission_granted(&origin, kind) {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.session.answer_permission(true);
            }
            return None;
        }
        Some((origin, kind))
    }

    /// Answer the active tab's pending permission prompt; a grant is remembered for
    /// the session so the same origin+capability won't re-prompt.
    fn answer_active_permission(&mut self, origin: &str, kind: u8, allow: bool) {
        if allow {
            self.grant_permission(origin, kind);
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.session.answer_permission(allow);
        }
    }

    /// Save (or update) a session-only login for `host`; replaces an existing entry
    /// with the same host+username. Blank host/username/password is ignored.
    fn save_login(&mut self, host: &str, username: &str, password: &str) {
        let host = host.trim().to_ascii_lowercase();
        let username = username.trim().to_owned();
        if host.is_empty() || username.is_empty() || password.is_empty() {
            return;
        }
        if let Some(existing) = self
            .session_logins
            .iter_mut()
            .find(|l| l.host == host && l.username == username)
        {
            existing.password = password.to_owned();
        } else {
            self.session_logins.push(StoredLogin {
                host,
                username,
                password: password.to_owned(),
            });
        }
    }

    /// The saved logins for `host` (lowercased), in save order.
    fn logins_for_host(&self, host: &str) -> Vec<&StoredLogin> {
        let host = host.trim().to_ascii_lowercase();
        self.session_logins
            .iter()
            .filter(|l| l.host == host)
            .collect()
    }

    /// Remove the saved login at `index` (manager delete).
    fn remove_login(&mut self, index: usize) {
        if index < self.session_logins.len() {
            self.session_logins.remove(index);
        }
    }

    /// Autofill the active tab's login form with a chosen credential (the engine
    /// injects the fill script). User-initiated only.
    fn fill_active_login(&mut self, username: String, password: String) {
        if let Some(tab) = self.active_tab() {
            tab.session.fill_login(username, password);
        }
        self.mark_active_tab_activity();
    }

    /// Fold an auto-captured login (engine-beaconed JSON `{origin, username,
    /// password}`) into a pending "Save password?" offer. Skipped if the exact
    /// credential is already stored, so a re-login never re-prompts.
    fn handle_login_capture(&mut self, body: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
            return;
        };
        let origin = v.get("origin").and_then(|x| x.as_str()).unwrap_or_default();
        let username = v
            .get("username")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .trim()
            .to_owned();
        let password = v
            .get("password")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        let host = host_of(origin).unwrap_or_default();
        if host.is_empty() || username.is_empty() || password.is_empty() {
            return;
        }
        if self
            .session_logins
            .iter()
            .any(|l| l.host == host && l.username == username && l.password == password)
        {
            return; // already saved — no offer
        }
        self.pending_login_save = Some(StoredLogin {
            host,
            username,
            password,
        });
    }

    fn compiled_request_filter_for_url(&self, url: &str) -> RequestFilter {
        let mut filter = self.compiled_request_filter();
        filter.set_page(url);
        filter
    }

    fn apply_adfilter_to_open_tabs(&mut self) {
        let store = self.adfilter_store.clone();
        let safe_browsing = SafeBrowsingBlocklist::from_hosts(&self.safe_browsing_hosts);
        for tab in &mut self.tabs {
            let mut filter =
                RequestFilter::from_store(&store).with_safe_browsing(safe_browsing.clone());
            filter.set_page(&tab.session.nav().url);
            tab.session.set_filter(filter);
        }
    }

    fn active_first_party(&self) -> Option<String> {
        let url = self.tabs.get(self.active)?.session.nav().url.trim();
        host_of(url)
    }

    fn active_site_blocking_enabled(&self) -> bool {
        self.active_first_party()
            .is_some_and(|host| !self.adfilter_store.allowlist().is_allowed(&host))
    }

    fn safe_browsing_summary(&self) -> String {
        if self.safe_browsing_hosts.is_empty() {
            "Safe browsing: no mesh-hosted unsafe hosts loaded".to_owned()
        } else {
            format!(
                "Safe browsing: {} mesh-hosted unsafe host{} loaded",
                self.safe_browsing_hosts.len(),
                if self.safe_browsing_hosts.len() == 1 {
                    ""
                } else {
                    "s"
                }
            )
        }
    }

    fn site_data_summary(&self) -> String {
        self.site_data.summary(self.active_first_party().as_deref())
    }

    fn update_site_data_from_tabs(&mut self) {
        let hosts = self
            .tabs
            .iter()
            .filter_map(|tab| host_of(tab.session.nav().url.trim()))
            .collect::<Vec<_>>();
        self.site_data
            .observe_open_tabs(hosts.iter().map(String::as_str), unix_ms());
    }

    /// Record the ACTIVE tab's committed navigation into the session-only history
    /// (B3). `record` dedupes the NavState `loading:true→false` churn and reloads,
    /// and back-fills the title as it arrives; new-tab/blank pages are skipped.
    fn record_history_from_active_tab(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let url = tab.session.nav().url.trim().to_owned();
        let title = tab.session.title().to_owned();
        if url.is_empty() || url == NEW_TAB_URL {
            return;
        }
        self.history.record(&url, &title, unix_ms());
    }

    fn set_active_site_blocking(&mut self, enabled: bool) {
        let Some(host) = self.active_first_party() else {
            return;
        };
        let now = unix_ms();
        if enabled {
            self.adfilter_store
                .block_site(&host, &local_hostname(), now);
            publish(ACTION_ADFILTER_BLOCK, &adfilter_domain_body(&host));
        } else {
            self.adfilter_store
                .allow_site(&host, &local_hostname(), now);
            publish(ACTION_ADFILTER_ALLOW, &adfilter_domain_body(&host));
        }
        self.apply_adfilter_to_open_tabs();
    }

    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "wired to synced browser policy follow-up")
    )]
    fn add_custom_filter_rules(&mut self, name: &str, raw: &str) {
        let name = name.trim();
        let raw = raw.trim();
        if name.is_empty() || raw.is_empty() {
            return;
        }
        self.adfilter_store
            .add_source(FilterListSource::custom(name, None, raw, unix_ms()));
        self.apply_adfilter_to_open_tabs();
    }

    fn set_safe_browsing_hosts(&mut self, hosts: impl IntoIterator<Item = impl AsRef<str>>) {
        self.safe_browsing_hosts = hosts
            .into_iter()
            .filter_map(|host| {
                let host = host.as_ref().trim().to_ascii_lowercase();
                (!host.is_empty()).then_some(host)
            })
            .collect();
        self.apply_adfilter_to_open_tabs();
    }

    /// Populate the safe-browsing blocklist from the operator-curated mesh policy
    /// file (`browser/safe-browsing-hosts.txt` under the workgroup root — the mackesd
    /// sync/operator writes it). Throttled; re-applies to open tabs only on a real
    /// change. This is the "mesh policy source" wiring that activates the blocklist.
    fn poll_safe_browsing_hosts(&mut self) {
        if self
            .safe_browsing_last_poll
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.safe_browsing_last_poll = Some(Instant::now());
        let path = default_workgroup_root().join(SAFE_BROWSING_HOSTS_PATH);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let hosts = parse_safe_browsing_hosts(&text);
        if hosts != self.safe_browsing_hosts {
            self.set_safe_browsing_hosts(hosts);
        }
    }
}

/// The operator-curated safe-browsing blocklist path, relative to the workgroup root.
const SAFE_BROWSING_HOSTS_PATH: &str = "browser/safe-browsing-hosts.txt";

/// Parse the safe-browsing blocklist file: one host per line, `#` comments and blank
/// lines skipped, hosts trimmed + lowercased. Pure so the parse is unit-tested.
fn parse_safe_browsing_hosts(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The `live-helper` spawn glue: creating live [`WebSession`]s by launching the
/// sandboxed `mde-web-preview` helper (the client crate's `live-helper` API) and
/// attaching them as tabs, behind the honest deployment gates (§7). All of this is
/// compiled out of the default portable build.
#[cfg(feature = "live-helper")]
impl WebState {
    /// Record the live seat's output size (device px) so the next helper spawn
    /// pre-sizes its frame channel to it. Called each frame from the shell's Browser
    /// arm just before [`Self::ensure_live_tab`], so the very first spawn already
    /// knows the real seat.
    ///
    /// The channel is `w * h * 4` bytes of shm, so each axis is clamped to
    /// [`MAX_CHANNEL_DIM`]. Pre-sizing to the seat (the ceiling of any panel-driven
    /// resize, since the Browser panel never exceeds the screen) means a live
    /// [`WebSession::resize`] that enlarges the CEF paint always fits the channel
    /// instead of being silently dropped (`FrameChannelError::TooLarge`) — growing
    /// the channel dynamically would need re-attaching a new fd across the pinned
    /// CEF ABI, so pre-sizing is the chosen, documented alternative (browser-1).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "seat extent is rounded then clamped into [1, MAX_CHANNEL_DIM]"
    )]
    pub(crate) fn note_seat_px(&mut self, ctx: &egui::Context) {
        let size = ctx.screen_rect().size() * ctx.pixels_per_point();
        let clamp = |v: f32| -> u32 {
            if v.is_finite() {
                v.round().clamp(1.0, MAX_CHANNEL_DIM as f32) as u32
            } else {
                INIT_W
            }
        };
        self.seat_px = (clamp(size.x), clamp(size.y));
    }

    /// Ensure a live browser tab exists — spawn the sandboxed helper on first open.
    /// The shell's Browser arm calls this each frame with the live seat verdict. A
    /// no-op once a tab is open, and the real `Command::spawn` is attempted at most
    /// once (a failure surfaces an honest notice, never a per-frame spawn-storm).
    pub(crate) fn ensure_live_tab(&mut self, seat_present: bool) {
        if !self.tabs.is_empty() || self.spawn_attempted {
            return;
        }
        self.restore_startup_session_once();
        self.drain_incoming_send_tabs();
        if !self.open_requested.is_empty() {
            self.spawn_attempted = true;
            self.drain_live_tab_requests(seat_present);
            return;
        }
        self.spawn_attempted = true;
        self.open_with(
            seat_present,
            self.engine,
            START_URL.to_owned(),
            helper_bin_path(self.engine),
            WebSession::spawn,
        );
    }

    /// Drain the visible tab strip's new-tab request into a real helper spawn.
    pub(crate) fn drain_live_tab_requests(&mut self, seat_present: bool) {
        while let Some(intent) = self.take_open_request() {
            match intent {
                TabOpenIntent::NewForeground(engine) => {
                    self.open_with(
                        seat_present,
                        engine,
                        START_URL.to_owned(),
                        helper_bin_path(engine),
                        WebSession::spawn,
                    );
                }
                TabOpenIntent::NewForegroundUrl { engine, url } => {
                    self.open_with(
                        seat_present,
                        engine,
                        url,
                        helper_bin_path(engine),
                        WebSession::spawn,
                    );
                }
            }
        }
    }

    /// Respawn the active crashed tab with a fresh live session (respawn-on-reload),
    /// drained by the Browser arm when [`Self::take_respawn_request`] fires. Driven
    /// by an explicit user Reload, so it is not rate-limited by the one-shot latch.
    pub(crate) fn respawn_live(&mut self) {
        // A tab was already open, so the seat gate is already proven live.
        let engine = self
            .tabs
            .get(self.active)
            .map_or(self.engine, |tab| tab.engine);
        if let Some(session) = self.make_session(
            true,
            engine,
            START_URL.to_owned(),
            helper_bin_path(engine),
            WebSession::spawn,
        ) {
            self.respawn_active_with_engine(session, engine);
        }
    }

    /// Testable core of [`Self::ensure_live_tab`]: attach a session from `spawn` as
    /// the first tab, applying the honest gates. Production passes
    /// [`WebSession::spawn`]; the tests inject a `testkit` factory so no real process
    /// is spawned (hermetic CI).
    fn open_with(
        &mut self,
        seat_present: bool,
        engine: BrowserEngine,
        url: String,
        helper_bin: std::path::PathBuf,
        spawn: impl FnOnce(&SpawnSpec) -> std::io::Result<WebSession>,
    ) {
        if let Some(session) = self.make_session(seat_present, engine, url, helper_bin, spawn) {
            self.push_session_with_engine(session, engine);
        }
    }

    /// Build one live session behind the honest gates (a usable seat · the helper
    /// binary installed · the spawn succeeding), or record a NAMED notice and return
    /// `None`. Never fakes a page, never panics, never hangs — a spawn failure
    /// surfaces its reason (§7).
    fn make_session(
        &mut self,
        seat_present: bool,
        engine: BrowserEngine,
        url: String,
        helper_bin: std::path::PathBuf,
        spawn: impl FnOnce(&SpawnSpec) -> std::io::Result<WebSession>,
    ) -> Option<WebSession> {
        if !seat_present {
            self.gate_notice =
                Some("The sandboxed browser needs a GPU seat — none is available here.".to_owned());
            return None;
        }
        if !helper_bin.exists() {
            self.gate_notice = Some(format!(
                "The sandboxed browser helper is not installed ({}).",
                helper_bin.display()
            ));
            return None;
        }
        if engine == BrowserEngine::Cef {
            if let Some(missing) = cef_runtime_missing_path() {
                self.gate_notice = Some(format!(
                    "The Chromium/CEF runtime is not installed (missing {}).",
                    missing.display()
                ));
                return None;
            }
        }
        // Pre-size the helper's frame channel to the live seat (device px) so a
        // later resize can grow the CEF paint up to the seat without overflowing
        // the channel (browser-1, item 3); falls back to (INIT_W, INIT_H) until a
        // real seat is seen via `note_seat_px`.
        let (width, height) = self.seat_px;
        let spec = SpawnSpec {
            helper_bin,
            url,
            width,
            height,
        };
        match spawn(&spec) {
            Ok(session) => {
                self.gate_notice = None;
                Some(session)
            }
            Err(e) => {
                self.gate_notice =
                    Some(format!("The sandboxed browser helper failed to start: {e}"));
                None
            }
        }
    }
}

/// The browser-reserved tab accelerators (Chrome's tab-strip keyboard UX),
/// live only while the Browser surface is painted — this runs from
/// [`web_panel`].
///
/// Every match CONSUMES the key from this frame's input, so a reserved
/// shortcut never leaks into chrome widgets or the page-canvas forwarding at
/// the bottom of the frame (`paint_body` clones `i.events` after this ran) —
/// the same reservation Chrome makes for Ctrl+T/W. Page-canvas keyboard focus
/// deliberately does NOT pause these: tab management stays reachable while
/// page typing is forwarded.
///
/// Tab-opening/closing accelerators (Ctrl+T / Ctrl+W / Ctrl+Shift+T) pause
/// while a chrome text field (omnibox / find bar / dashboard search) owned
/// keyboard focus on the last painted frame — closing the tab out of an
/// in-progress edit would surprise, and egui's own TextEdit binds Ctrl+W as
/// delete-previous-word, which the pause preserves. Tab CYCLING
/// (Ctrl+Tab / Ctrl+digit) stays
/// live during edits, exactly like desktop browsers — and deliberately so:
/// egui's own focus traversal walks widget focus on Tab presses, so a cycling
/// shortcut gated on text focus could dead-end itself once that walk reaches
/// the omnibox.
fn handle_tab_keyboard(ctx: &egui::Context, state: &mut WebState) {
    const CTRL: egui::Modifiers = egui::Modifiers::CTRL;
    const CTRL_SHIFT: egui::Modifiers = egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT);
    // F11 toggles immersive/fullscreen mode; Esc leaves it. Handled before the
    // edit-focus gate so the immersive view is always escapable, even mid-typing.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F11)) {
        state.fullscreen = !state.fullscreen;
    }
    if state.fullscreen
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        state.fullscreen = false;
    }
    // ORDER MATTERS: `consume_key` matches modifiers logically (an EXTRA
    // Shift is ignored — egui's documented behaviour), so the Ctrl+Shift
    // variants must be consumed before their plain-Ctrl counterparts or
    // Ctrl+Shift+T would trigger "new tab" instead of "reopen".
    if !state.chrome_edit_focus {
        if ctx.input_mut(|i| i.consume_key(CTRL_SHIFT, egui::Key::T)) {
            state.restore_closed_tab();
        }
        if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::T)) {
            state.request_new_tab(state.engine);
        }
        if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::W)) {
            state.close_tab(state.active);
        }
    }
    if ctx.input_mut(|i| i.consume_key(CTRL_SHIFT, egui::Key::Tab)) {
        state.select_prev_tab();
    }
    if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::Tab)) {
        state.select_next_tab();
    }
    // Ctrl+1..Ctrl+8 activate the Nth tab (out-of-range is ignored by
    // `select_tab`); Ctrl+9 activates the LAST tab — the Chrome convention.
    const DIGITS: [egui::Key; 8] = [
        egui::Key::Num1,
        egui::Key::Num2,
        egui::Key::Num3,
        egui::Key::Num4,
        egui::Key::Num5,
        egui::Key::Num6,
        egui::Key::Num7,
        egui::Key::Num8,
    ];
    for (nth, key) in DIGITS.into_iter().enumerate() {
        if ctx.input_mut(|i| i.consume_key(CTRL, key)) {
            state.select_tab(nth);
        }
    }
    if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::Num9)) {
        if let Some(last) = state.tabs.len().checked_sub(1) {
            state.select_tab(last);
        }
    }
}

/// Render the Browser surface into `ui`: poll every tab, upload any fresh frame on
/// the active tab, draw the navigation chrome, and paint the body (or the honest
/// crashed / loading / gated states).
pub(crate) fn web_panel(ui: &mut egui::Ui, state: &mut WebState) {
    // Tab-strip keyboard UX: consume the browser-reserved shortcuts FIRST so
    // neither chrome widgets nor the page-canvas forwarding in `paint_body`
    // ever see them.
    handle_tab_keyboard(ui.ctx(), state);
    state.poll_suggestions();
    state.poll_downloads();
    state.poll_incoming_send_tabs();
    state.poll_voice_command_results();
    state.poll_speech_statuses();
    state.poll_passkey_status();
    state.poll_passkey_results();
    state.poll_share_results();
    state.poll_translation_results();
    state.poll_offline_cache_results();
    state.poll_security_update_status();
    state.poll_bookmarks_collection();
    state.poll_safe_browsing_hosts();
    state.suspend_idle_tabs(Instant::now());

    // 1. Poll every tab so background tabs keep receiving — and so ONE tab's crash
    //    is observed here without disturbing the others (per-session isolation).
    let mut pdf_events = Vec::new();
    let mut page_text_events = Vec::new();
    let mut page_scrape_events = Vec::new();
    let mut passkey_events = Vec::new();
    let mut popup_opens = Vec::new();
    let mut download_submits = Vec::new();
    let mut login_captures = Vec::new();
    for (idx, tab) in state.tabs.iter_mut().enumerate() {
        if tab.idle_suspended && idx != state.active {
            continue;
        }
        tab.session.poll();
        for event in tab.session.drain_pdf_events() {
            pdf_events.push((event.path, event.ok));
        }
        for event in tab.session.drain_page_text_events() {
            page_text_events.push((event.id, event.text));
        }
        for event in tab.session.drain_page_scrape_events() {
            page_scrape_events.push((event.id, event.body));
        }
        for event in tab.session.drain_passkey_events() {
            passkey_events.push((idx, tab.engine, event.body));
        }
        // window.open / target=_blank the engine cancelled → open as a real tab
        // on the same engine (the popup-producer chain, EventMsg::PopupRequested).
        for request in tab.session.drain_popup_requests() {
            popup_opens.push((tab.engine, request.url));
        }
        // Downloads the engine intercepted (B2) → submit to the mesh Transfers
        // ledger (the daemon fetches into the mesh share).
        for event in tab.session.drain_download_events() {
            download_submits.push((event.id, event.url, event.filename));
        }
        // A submitted login (auto-capture) → offer to save it (session-only).
        for body in tab.session.drain_login_captures() {
            login_captures.push(body);
        }
    }
    for (engine, url) in popup_opens {
        state.request_new_tab_with_url(engine, url);
    }
    for (id, url, filename) in download_submits {
        state.submit_download_to_ledger(id, &url, &filename);
    }
    let mut pdf_notice = None;
    for (path, ok) in pdf_events {
        pdf_notice = Some(state.handle_pdf_event(path, ok));
    }
    if let Some(notice) = pdf_notice {
        state.capture_notice = Some(notice);
    }
    for (id, text) in page_text_events {
        state.handle_page_text_event(id, text);
    }
    for (id, body) in page_scrape_events {
        state.handle_page_scrape_event(id, body);
    }
    for (tab_index, engine, body) in passkey_events {
        state.handle_passkey_event(tab_index, engine, &body);
    }
    for body in login_captures {
        state.handle_login_capture(&body);
    }
    state.poll_spellcheck();
    // Engine-driven navigations (redirects, page scripts) land in the address
    // bar here — the tab poll above has already drained this frame's nav
    // events, and the focus guard keeps operator edits intact.
    state.sync_address_on_engine_nav();
    state.poll_session_snapshot();

    // 2. Upload the active tab's pending frame — ONLY when one is present, so an
    //    idle page never triggers a re-upload.
    if let Some(tab) = state.active_tab() {
        if let Some(img) = tab.session.take_frame() {
            // Share one `Arc<ColorImage>` between the retained CPU frame and the
            // GPU upload instead of deep-copying the full-resolution pixels on
            // every decoded frame: the retain is a refcount bump, and the upload
            // moves the same `Arc` (`egui::ImageData` already stores it as one).
            let img = std::sync::Arc::new(img);
            tab.last_frame = Some(img.clone());
            match tab.texture.as_mut() {
                Some(handle) => handle.set(img, BROWSER_TEX),
                None => tab.texture = Some(ui.ctx().load_texture("web-preview", img, BROWSER_TEX)),
            }
        }
    }
    install_browser_accessibility(ui.ctx(), ui.max_rect(), state);

    // 3. The shared MENUBAR-ALL top bar — the UPPERCASE BROWSER title, the real
    //    WebSession menus (Edit / View / History / Bookmarks), and the live status
    //    cluster. It COMPLEMENTS the navigation chrome below (never replaces it —
    //    the address bar + back/forward/reload buttons stay), the same model→seam
    //    pattern every other surface uses (design: `docs/design/menubar-all.md`).
    if let Some(action) = menubar::show(state, ui) {
        menubar::apply(ui.ctx(), state, action);
    }
    // The accelerator + omnibox-sync guards above read LAST frame's chrome
    // text-field focus; re-collect it from the chrome widgets painted below
    // (the omnibox, the find bar, and the dashboard search each OR into it).
    state.chrome_edit_focus = false;
    state.omnibox_focused = false;
    ui.add_space(CHROME_GAP);

    // Immersive/fullscreen mode: only the page body renders — no tab strip, nav bar,
    // bookmarks, or drawers. Triggered by F11 (manual, state.fullscreen) OR the page
    // itself entering HTML5 fullscreen (on_fullscreen_mode_change → the active
    // session reports it). F11/Esc exits the manual mode; the page exit clears its own.
    let page_fullscreen = state
        .tabs
        .get(state.active)
        .is_some_and(|tab| tab.session.fullscreen());
    if state.fullscreen || page_fullscreen {
        active_body(ui, state);
        return;
    }

    if state.vertical_tabs {
        ui.horizontal(|ui| {
            tab_strip(ui, state);
            ui.add_space(CHROME_GAP);
            ui.vertical(|ui| {
                // The navigation chrome (back / forward / reload / address bar),
                // wired to the active session's control socket.
                nav_chrome(ui, state);
                bookmarks_bar(ui, state);
                find_chrome(ui, state);
                insecure_prompt(ui, state);
                capture_notice(ui, state);
                qr_share_drawer(ui, state);
                spellcheck_drawer(ui, state);
                speech_status_drawer(ui, state);
                security_update_drawer(ui, state);
                translation_drawer(ui, state);
                offline_cache_drawer(ui, state);
                print_settings_drawer(ui, state);
                site_styles_drawer(ui, state);
                downloads_drawer(ui, state);
                history_drawer(ui, state);
                ui.add_space(CHROME_GAP);
                active_body(ui, state);
            });
        });
    } else {
        // First-class tab strip (BROWSER-DD-2): switch/close existing isolated
        // sessions and expose a real new-tab intent for the live-helper path.
        tab_strip(ui, state);
        ui.add_space(CHROME_GAP);

        // The navigation chrome (back / forward / reload / address bar), wired to
        // the active session's control socket.
        nav_chrome(ui, state);
        bookmarks_bar(ui, state);
        find_chrome(ui, state);
        insecure_prompt(ui, state);
        capture_notice(ui, state);
        qr_share_drawer(ui, state);
        spellcheck_drawer(ui, state);
        speech_status_drawer(ui, state);
        security_update_drawer(ui, state);
        translation_drawer(ui, state);
        offline_cache_drawer(ui, state);
        print_settings_drawer(ui, state);
        site_styles_drawer(ui, state);
        downloads_drawer(ui, state);
        history_drawer(ui, state);
        ui.add_space(CHROME_GAP);
        active_body(ui, state);
    }
}

fn active_body(ui: &mut egui::Ui, state: &mut WebState) {
    // Read the active tab's status first so the crashed/cert-error arms can
    // mutate `state` (respawn flag, back/close) without holding a `&mut Tab`
    // borrow of it.
    let active = state.active;
    // Safe-browsing: a top-level navigation to an unsafe host shows a full-page
    // "unsafe site" interstitial (the request was already dropped upstream). Taken
    // before the normal body, mirroring the cert-error precedence.
    let sb_block = state
        .tabs
        .get(active)
        .and_then(|t| t.session.safe_browsing_block().map(str::to_owned));
    if let Some(url) = sb_block {
        if safe_browsing_interstitial_body(ui, &url) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_back();
            }
            state.mark_active_tab_activity();
        }
        return;
    }
    // Permission prompt: an origin's pending capability request renders a small bar
    // atop the page (Allow/Block). A capability granted earlier this session
    // auto-allows inside `pending_permission_prompt` and never reaches here.
    // Guard: NEVER paint the bar over a crash/cert interstitial that will replace the
    // page below — a blocked/crashed page can't have raised the request, but keep the
    // precedence honest defensively (safe-browsing already returned above).
    let interstitial_below = state
        .tabs
        .get(active)
        .is_some_and(|t| t.session.is_crashed() || t.session.cert_error().is_some());
    if !interstitial_below {
        if let Some((origin, kind)) = state.pending_permission_prompt() {
            if let Some(allow) = permission_prompt_bar(ui, &origin, kind) {
                state.answer_active_permission(&origin, kind, allow);
            }
        }
        // "Save password?" offer for an auto-captured login submit.
        if let Some(pending) = state.pending_login_save.clone() {
            match login_save_prompt_bar(ui, &pending.host, &pending.username) {
                Some(true) => {
                    state.save_login(&pending.host, &pending.username, &pending.password);
                    state.pending_login_save = None;
                }
                Some(false) => state.pending_login_save = None,
                None => {}
            }
        }
    }
    let status = state.tabs.get(active).map(|t| {
        let is_crashed = t.session.is_crashed();
        let cert_error = t.session.cert_error().cloned();
        // `shows_cert_interstitial` is the single source of truth for the
        // crashed-wins precedence; fold its verdict into the option here so
        // the match arms below don't have to re-derive the ordering.
        let cert_interstitial =
            shows_cert_interstitial(is_crashed, cert_error.as_ref()).then_some(cert_error);
        (
            is_crashed,
            cert_interstitial.flatten(),
            t.texture.is_some(),
            is_new_tab_url(t.session.nav().url.trim()),
            crash_reason(&t.session),
            t.session.nav().can_back,
        )
    });
    match status {
        Some((true, _, _, _, reason, _)) => {
            if let Some(snapshot) = state.offline_cache_fallback_for_unavailable().cloned() {
                cached_offline_body(ui, &snapshot, Some(reason.as_str()));
            } else {
                crashed_body(ui, reason, &mut state.respawn_requested);
            }
        }
        // The engine blocks the navigation by default on a TLS/certificate
        // error (§ cert-error ENGINE half) and hands the shell a `CertError`;
        // this takes precedence over a normal frame/dashboard the same way
        // `is_crashed` does — checked right beside it, one arm down.
        Some((false, Some(err), _, _, _, can_back)) => {
            if cert_error_body(ui, &err, can_back) {
                match cert_error_back_action(can_back) {
                    CertErrorBackAction::GoBack => {
                        if let Some(tab) = state.active_tab() {
                            tab.session.go_back();
                        }
                        state.mark_active_tab_activity();
                    }
                    CertErrorBackAction::CloseTab => state.close_tab(active),
                }
            }
        }
        Some((false, None, _, true, _, _)) => new_tab_dashboard(ui, state),
        Some((false, None, true, false, _, _)) => paint_body(ui, state, active),
        Some((false, None, false, false, _, _)) => {
            // Connected, no first frame yet — an honest loading note, never a blank.
            centered(ui, |ui| {
                muted_note(ui, "Loading the page\u{2026}");
            });
        }
        None => {
            let cached = state.offline_cache_fallback_for_unavailable().cloned();
            // The honest gated body — a `live-helper` build shows the NAMED gate
            // notice (no seat · helper absent · spawn failed) when one is set; the
            // default build always shows the standard gated caption (§7).
            #[cfg(feature = "live-helper")]
            let notice = state.gate_notice.as_deref();
            #[cfg(not(feature = "live-helper"))]
            let notice: Option<&str> = None;
            if let Some(snapshot) = cached {
                cached_offline_body(ui, &snapshot, notice);
            } else {
                empty_body(ui, notice);
            }
        }
    }
}

fn tab_strip(ui: &mut egui::Ui, state: &mut WebState) {
    if state.vertical_tabs {
        vertical_tab_strip(ui, state);
    } else {
        horizontal_tab_strip(ui, state);
    }
}

fn horizontal_tab_strip(ui: &mut egui::Ui, state: &mut WebState) {
    let mut select: Option<usize> = None;
    let mut close: Option<usize> = None;
    let mut move_tab: Option<(usize, usize)> = None;
    let mut group_tab: Option<usize> = None;
    let mut ungroup_tab_idx: Option<usize> = None;
    let mut mute_tab: Option<(usize, bool)> = None;
    let mut force_dark_tab: Option<(usize, bool)> = None;
    let mut reader_tab: Option<(usize, bool)> = None;
    let mut user_scripts_tab: Option<(usize, bool)> = None;
    let mut container_tab: Option<(usize, ContainerProfile)> = None;
    let mut display_tab: Option<(usize, DisplayTarget)> = None;
    let mut pin_tab: Option<(usize, bool)> = None;
    let mut duplicate_tab_idx: Option<usize> = None;
    let mut close_others_idx: Option<usize> = None;
    let mut close_right_idx: Option<usize> = None;

    // Overflow (BROWSER tabstrip): pills shrink toward a floor as they multiply;
    // once at the floor the strip scrolls horizontally in ONE row instead of
    // wrapping onto stacked rows.
    let pill_width = horizontal_tab_pill_width(ui.available_width(), state.tabs.len());

    // Resolve/cache each tab's favicon texture BEFORE the (immutable) pill loop
    // below — see `resolve_tab_favicon_textures`.
    let favicon_textures = resolve_tab_favicon_textures(ui.ctx(), &mut state.tabs);

    // Scroll the active pill into view only when the active tab actually CHANGED,
    // so the operator can still scroll the strip freely while a tab stays selected.
    let last_active_id = egui::Id::new("browser-horizontal-tabs-last-active");
    let active_changed =
        ui.ctx().data(|d| d.get_temp::<usize>(last_active_id)) != Some(state.active);

    // Drag-reorder bookkeeping: every pill's laid-out rect (in tab order), plus the
    // pill under a settled drag and where it was dropped.
    let mut pill_rects: Vec<(usize, egui::Rect)> = Vec::new();
    let mut drag_from: Option<usize> = None;
    let mut drop_pointer: Option<egui::Pos2> = None;

    egui::ScrollArea::horizontal()
        .id_salt("browser-horizontal-tabs")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (idx, tab) in state.tabs.iter().enumerate() {
                    let active = idx == state.active;
                    // Pinned tabs collapse to a compact favicon-only pill (no title).
                    let label = if tab.pinned {
                        String::new()
                    } else {
                        tab_label(tab)
                    };
                    let pill_w = if tab.pinned {
                        CHROME_TAB_PINNED_W
                    } else {
                        pill_width
                    };
                    tab_favicon_image(ui, favicon_textures.get(idx).and_then(Option::as_ref));
                    let tab_response = tab_pill_sized(ui, &label, active, pill_w);
                    pill_rects.push((idx, tab_response.rect));
                    if tab_response.clicked() {
                        select = Some(idx);
                    }
                    // Middle-click closes the tab under the pointer (the ubiquitous
                    // desktop-browser gesture) — same seam as the inline × button.
                    if tab_response.middle_clicked() {
                        close = Some(idx);
                    }
                    // A settled horizontal drag reorders the tab to where it was
                    // dropped; egui's own click/drag threshold keeps a plain click
                    // (activate) and a middle-click (close) intact.
                    if tab_response.drag_stopped() {
                        drag_from = Some(idx);
                        drop_pointer = tab_response.interact_pointer_pos();
                    }
                    if active && active_changed {
                        tab_response.scroll_to_me(Some(egui::Align::Center));
                    }
                    // Tab-group indicator: a colored strip along the pill's bottom edge
                    // (Chrome's grouped-tab color band), painted from the response rect
                    // so it never disturbs the click/drag interaction above.
                    if let Some(color) = tab
                        .group
                        .and_then(|g| state.tab_groups.get(g))
                        .map(|g| g.color)
                    {
                        let r = tab_response.rect;
                        ui.painter().rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(r.left() + 2.0, r.bottom() - 2.0),
                                egui::pos2(r.right() - 2.0, r.bottom()),
                            ),
                            0.0,
                            color,
                        );
                    }
                    tab_response
                        .on_hover_ui(|ui| tab_hover_card(ui, tab))
                        .context_menu(|ui| {
                            if ui
                                .add_enabled(idx > 0, compact_menu_item("Move tab left"))
                                .clicked()
                            {
                                move_tab = Some((idx, idx - 1));
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(
                                    idx + 1 < state.tabs.len(),
                                    compact_menu_item("Move tab right"),
                                )
                                .clicked()
                            {
                                move_tab = Some((idx, idx + 1));
                                ui.close_menu();
                            }
                            let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
                            if ui.add(compact_menu_item(pin_label)).clicked() {
                                pin_tab = Some((idx, !tab.pinned));
                                ui.close_menu();
                            }
                            if ui.add(compact_menu_item("Duplicate tab")).clicked() {
                                duplicate_tab_idx = Some(idx);
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(
                                    state.tabs.len() > 1,
                                    compact_menu_item("Close other tabs"),
                                )
                                .clicked()
                            {
                                close_others_idx = Some(idx);
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(
                                    idx + 1 < state.tabs.len(),
                                    compact_menu_item("Close tabs to the right"),
                                )
                                .clicked()
                            {
                                close_right_idx = Some(idx);
                                ui.close_menu();
                            }
                            if tab.group.is_none() {
                                if ui.add(compact_menu_item("Add tab to new group")).clicked() {
                                    group_tab = Some(idx);
                                    ui.close_menu();
                                }
                            } else if ui.add(compact_menu_item("Remove from group")).clicked() {
                                ungroup_tab_idx = Some(idx);
                                ui.close_menu();
                            }
                            let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                            if ui.add(compact_menu_item(mute_label)).clicked() {
                                mute_tab = Some((idx, !tab.muted));
                                ui.close_menu();
                            }
                            let dark_label = if tab.force_dark {
                                "Disable force dark"
                            } else {
                                "Enable force dark"
                            };
                            if ui.add(compact_menu_item(dark_label)).clicked() {
                                force_dark_tab = Some((idx, !tab.force_dark));
                                ui.close_menu();
                            }
                            let reader_label = if tab.reader_mode {
                                "Disable reader mode"
                            } else {
                                "Enable reader mode"
                            };
                            if ui.add(compact_menu_item(reader_label)).clicked() {
                                reader_tab = Some((idx, !tab.reader_mode));
                                ui.close_menu();
                            }
                            let scripts_label = if tab.user_scripts {
                                "Disable userscripts"
                            } else {
                                "Enable userscripts"
                            };
                            if ui.add(compact_menu_item(scripts_label)).clicked() {
                                user_scripts_tab = Some((idx, !tab.user_scripts));
                                ui.close_menu();
                            }
                            ui.separator();
                            for container in ContainerProfile::ALL {
                                if ui
                                    .add_enabled(
                                        tab.container != container,
                                        compact_menu_item(container.label()),
                                    )
                                    .clicked()
                                {
                                    container_tab = Some((idx, container));
                                    ui.close_menu();
                                }
                            }
                            ui.separator();
                            for display_target in DisplayTarget::ALL {
                                if ui
                                    .add_enabled(
                                        tab.display_target != display_target,
                                        compact_menu_item(display_target.label()),
                                    )
                                    .clicked()
                                {
                                    display_tab = Some((idx, display_target));
                                    ui.close_menu();
                                }
                            }
                            if ui.add(compact_menu_item("Close tab")).clicked() {
                                close = Some(idx);
                                ui.close_menu();
                            }
                        });
                    // Speaker glyph for an audible/muted tab, click-to-mute.
                    if let Some(audio) = tab_audio_glyph(ui, tab.session.audible(), tab.muted) {
                        if audio.clicked() {
                            mute_tab = Some((idx, !tab.muted));
                        }
                    }
                    // Pinned tabs hide the inline × (Chrome's affordance); they
                    // still close via middle-click or the context menu.
                    if !tab.pinned && inline_close_button(ui).clicked() {
                        close = Some(idx);
                    }
                }
                engine_new_tab_buttons(ui, state, false);
                tab_search_menu(ui, state);
            });
        });

    ui.ctx()
        .data_mut(|d| d.insert_temp(last_active_id, state.active));

    // Resolve a settled drag to a concrete reorder against the laid-out pills.
    if let (Some(from), Some(pointer)) = (drag_from, drop_pointer) {
        if let Some(to) = tab_drag_target_index(&pill_rects, pointer, TabAxis::Horizontal) {
            if to != from {
                move_tab = Some((from, to));
            }
        }
    }

    #[cfg(test)]
    {
        let rects: Vec<egui::Rect> = pill_rects.iter().map(|(_, r)| *r).collect();
        ui.ctx()
            .data_mut(|d| d.insert_temp(tab_pill_rects_id(), rects));
    }

    if let Some((idx, muted)) = mute_tab {
        state.select_tab(idx);
        state.set_active_tab_muted(muted);
    } else if let Some((idx, enabled)) = force_dark_tab {
        state.select_tab(idx);
        state.set_active_tab_force_dark(enabled);
    } else if let Some((idx, enabled)) = reader_tab {
        state.select_tab(idx);
        state.set_active_tab_reader_mode(enabled);
    } else if let Some((idx, enabled)) = user_scripts_tab {
        state.select_tab(idx);
        state.set_active_tab_user_scripts(enabled);
    } else if let Some((idx, container)) = container_tab {
        state.select_tab(idx);
        state.set_active_tab_container(container);
    } else if let Some((idx, display_target)) = display_tab {
        state.select_tab(idx);
        state.set_active_tab_display_target(display_target);
    } else if let Some((idx, pinned)) = pin_tab {
        state.set_tab_pinned(idx, pinned);
    } else if let Some(idx) = duplicate_tab_idx {
        state.duplicate_tab(idx);
    } else if let Some(idx) = close_others_idx {
        state.close_other_tabs(idx);
    } else if let Some(idx) = close_right_idx {
        state.close_tabs_to_the_right(idx);
    } else if let Some(idx) = group_tab {
        state.new_group_from_tab(idx);
    } else if let Some(idx) = ungroup_tab_idx {
        state.ungroup_tab(idx);
    } else if let Some((from, to)) = move_tab {
        state.move_tab(from, to);
    } else if let Some(idx) = close {
        state.close_tab(idx);
    } else if let Some(idx) = select {
        state.select_tab(idx);
    }
}

fn vertical_tab_strip(ui: &mut egui::Ui, state: &mut WebState) {
    let mut select: Option<usize> = None;
    let mut close: Option<usize> = None;
    let mut move_tab: Option<(usize, usize)> = None;
    let mut group_tab: Option<usize> = None;
    let mut ungroup_tab_idx: Option<usize> = None;
    let mut mute_tab: Option<(usize, bool)> = None;
    let mut force_dark_tab: Option<(usize, bool)> = None;
    let mut reader_tab: Option<(usize, bool)> = None;
    let mut user_scripts_tab: Option<(usize, bool)> = None;
    let mut container_tab: Option<(usize, ContainerProfile)> = None;
    let mut display_tab: Option<(usize, DisplayTarget)> = None;
    let mut pin_tab: Option<(usize, bool)> = None;
    let mut duplicate_tab_idx: Option<usize> = None;
    let mut close_others_idx: Option<usize> = None;
    let mut close_right_idx: Option<usize> = None;

    // Drag-reorder bookkeeping mirrors the horizontal strip, but the drop point is
    // matched along Y — a vertical drag reorders the stacked pills.
    let mut pill_rects: Vec<(usize, egui::Rect)> = Vec::new();
    let mut drag_from: Option<usize> = None;
    let mut drop_pointer: Option<egui::Pos2> = None;

    // Resolve/cache each tab's favicon texture BEFORE the (immutable) pill loop
    // below — see `resolve_tab_favicon_textures`.
    let favicon_textures = resolve_tab_favicon_textures(ui.ctx(), &mut state.tabs);

    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_width(184.0);
            egui::ScrollArea::vertical()
                .id_salt("browser-vertical-tabs")
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    for (idx, tab) in state.tabs.iter().enumerate() {
                        let active = idx == state.active;
                        // Pinned tabs collapse to a compact favicon-only pill.
                        let label = if tab.pinned {
                            String::new()
                        } else {
                            tab_label(tab)
                        };
                        ui.horizontal(|ui| {
                            tab_favicon_image(
                                ui,
                                favicon_textures.get(idx).and_then(Option::as_ref),
                            );
                            let width = if tab.pinned {
                                CHROME_TAB_PINNED_W
                            } else {
                                (ui.available_width() - CHROME_TAB_CLOSE - CHROME_GAP)
                                    .max(CHROME_NEW_TAB_W)
                            };
                            let resp = tab_pill_sized(ui, &label, active, width);
                            pill_rects.push((idx, resp.rect));
                            if resp.clicked() {
                                select = Some(idx);
                            }
                            // Middle-click closes this tab — same gesture as the
                            // horizontal strip.
                            if resp.middle_clicked() {
                                close = Some(idx);
                            }
                            // A settled vertical drag reorders this pill to where it
                            // was dropped (matched along Y).
                            if resp.drag_stopped() {
                                drag_from = Some(idx);
                                drop_pointer = resp.interact_pointer_pos();
                            }
                            // Tab-group indicator: a colored strip along the pill's LEFT
                            // edge (the vertical-strip analogue of the horizontal band).
                            if let Some(color) = tab
                                .group
                                .and_then(|g| state.tab_groups.get(g))
                                .map(|g| g.color)
                            {
                                let r = resp.rect;
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(r.left(), r.top() + 2.0),
                                        egui::pos2(r.left() + 2.0, r.bottom() - 2.0),
                                    ),
                                    0.0,
                                    color,
                                );
                            }
                            resp.on_hover_text(tab_hover(tab)).context_menu(|ui| {
                                if ui
                                    .add_enabled(idx > 0, compact_menu_item("Move tab up"))
                                    .clicked()
                                {
                                    move_tab = Some((idx, idx - 1));
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        idx + 1 < state.tabs.len(),
                                        compact_menu_item("Move tab down"),
                                    )
                                    .clicked()
                                {
                                    move_tab = Some((idx, idx + 1));
                                    ui.close_menu();
                                }
                                let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
                                if ui.add(compact_menu_item(pin_label)).clicked() {
                                    pin_tab = Some((idx, !tab.pinned));
                                    ui.close_menu();
                                }
                                if ui.add(compact_menu_item("Duplicate tab")).clicked() {
                                    duplicate_tab_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        state.tabs.len() > 1,
                                        compact_menu_item("Close other tabs"),
                                    )
                                    .clicked()
                                {
                                    close_others_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        idx + 1 < state.tabs.len(),
                                        compact_menu_item("Close tabs to the right"),
                                    )
                                    .clicked()
                                {
                                    close_right_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if tab.group.is_none() {
                                    if ui.add(compact_menu_item("Add tab to new group")).clicked() {
                                        group_tab = Some(idx);
                                        ui.close_menu();
                                    }
                                } else if ui.add(compact_menu_item("Remove from group")).clicked() {
                                    ungroup_tab_idx = Some(idx);
                                    ui.close_menu();
                                }
                                let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                                if ui.add(compact_menu_item(mute_label)).clicked() {
                                    mute_tab = Some((idx, !tab.muted));
                                    ui.close_menu();
                                }
                                let dark_label = if tab.force_dark {
                                    "Disable force dark"
                                } else {
                                    "Enable force dark"
                                };
                                if ui.add(compact_menu_item(dark_label)).clicked() {
                                    force_dark_tab = Some((idx, !tab.force_dark));
                                    ui.close_menu();
                                }
                                let reader_label = if tab.reader_mode {
                                    "Disable reader mode"
                                } else {
                                    "Enable reader mode"
                                };
                                if ui.add(compact_menu_item(reader_label)).clicked() {
                                    reader_tab = Some((idx, !tab.reader_mode));
                                    ui.close_menu();
                                }
                                let scripts_label = if tab.user_scripts {
                                    "Disable userscripts"
                                } else {
                                    "Enable userscripts"
                                };
                                if ui.add(compact_menu_item(scripts_label)).clicked() {
                                    user_scripts_tab = Some((idx, !tab.user_scripts));
                                    ui.close_menu();
                                }
                                ui.separator();
                                for container in ContainerProfile::ALL {
                                    if ui
                                        .add_enabled(
                                            tab.container != container,
                                            compact_menu_item(container.label()),
                                        )
                                        .clicked()
                                    {
                                        container_tab = Some((idx, container));
                                        ui.close_menu();
                                    }
                                }
                                ui.separator();
                                for display_target in DisplayTarget::ALL {
                                    if ui
                                        .add_enabled(
                                            tab.display_target != display_target,
                                            compact_menu_item(display_target.label()),
                                        )
                                        .clicked()
                                    {
                                        display_tab = Some((idx, display_target));
                                        ui.close_menu();
                                    }
                                }
                                if ui.add(compact_menu_item("Close tab")).clicked() {
                                    close = Some(idx);
                                    ui.close_menu();
                                }
                            });
                            // Speaker glyph for an audible/muted tab, click-to-mute.
                            if let Some(audio) =
                                tab_audio_glyph(ui, tab.session.audible(), tab.muted)
                            {
                                if audio.clicked() {
                                    mute_tab = Some((idx, !tab.muted));
                                }
                            }
                            // Pinned tabs hide the × (close via middle-click / menu).
                            if !tab.pinned && inline_close_button(ui).clicked() {
                                close = Some(idx);
                            }
                        });
                    }
                    engine_new_tab_buttons(ui, state, true);
                    tab_search_menu(ui, state);
                });
        });

    // Resolve a settled vertical drag to a concrete reorder against the pills.
    if let (Some(from), Some(pointer)) = (drag_from, drop_pointer) {
        if let Some(to) = tab_drag_target_index(&pill_rects, pointer, TabAxis::Vertical) {
            if to != from {
                move_tab = Some((from, to));
            }
        }
    }

    #[cfg(test)]
    {
        let rects: Vec<egui::Rect> = pill_rects.iter().map(|(_, r)| *r).collect();
        ui.ctx()
            .data_mut(|d| d.insert_temp(tab_pill_rects_id(), rects));
    }

    if let Some((idx, muted)) = mute_tab {
        state.select_tab(idx);
        state.set_active_tab_muted(muted);
    } else if let Some((idx, enabled)) = force_dark_tab {
        state.select_tab(idx);
        state.set_active_tab_force_dark(enabled);
    } else if let Some((idx, enabled)) = reader_tab {
        state.select_tab(idx);
        state.set_active_tab_reader_mode(enabled);
    } else if let Some((idx, enabled)) = user_scripts_tab {
        state.select_tab(idx);
        state.set_active_tab_user_scripts(enabled);
    } else if let Some((idx, container)) = container_tab {
        state.select_tab(idx);
        state.set_active_tab_container(container);
    } else if let Some((idx, display_target)) = display_tab {
        state.select_tab(idx);
        state.set_active_tab_display_target(display_target);
    } else if let Some((idx, pinned)) = pin_tab {
        state.set_tab_pinned(idx, pinned);
    } else if let Some(idx) = duplicate_tab_idx {
        state.duplicate_tab(idx);
    } else if let Some(idx) = close_others_idx {
        state.close_other_tabs(idx);
    } else if let Some(idx) = close_right_idx {
        state.close_tabs_to_the_right(idx);
    } else if let Some(idx) = group_tab {
        state.new_group_from_tab(idx);
    } else if let Some(idx) = ungroup_tab_idx {
        state.ungroup_tab(idx);
    } else if let Some((from, to)) = move_tab {
        state.move_tab(from, to);
    } else if let Some(idx) = close {
        state.close_tab(idx);
    } else if let Some(idx) = select {
        state.select_tab(idx);
    }
}

/// Case-insensitive match of `query` against each tab's title AND committed URL;
/// returns the matching tab indices in strip order. An empty/blank query matches
/// everything (the full list). Pure — the tab-search dropdown and its test share it.
fn matching_tab_indices(tabs: &[Tab], query: &str) -> Vec<usize> {
    let q = query.trim().to_ascii_lowercase();
    tabs.iter()
        .enumerate()
        .filter(|(_, tab)| {
            q.is_empty()
                || tab.session.title().to_ascii_lowercase().contains(&q)
                || tab.session.nav().url.to_ascii_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// A one-line label for a tab-search result row: the page title, falling back to
/// the URL, then "New tab" — no state dot (the dropdown is a chooser, not the strip).
fn tab_search_row_label(tab: &Tab) -> String {
    let title = tab.session.title().trim();
    if !title.is_empty() {
        return ellipsize(title, 48);
    }
    let url = tab.session.nav().url.trim();
    if url.is_empty() {
        "New tab".to_owned()
    } else {
        ellipsize(url, 48)
    }
}

/// The tab-search dropdown (Chrome's "Search tabs" ⌄): a 🔍 menu button opening a
/// live-filtered, clickable list of every open tab. Selecting a row activates that
/// tab and clears the query. Pure list logic lives in [`matching_tab_indices`].
fn tab_search_menu(ui: &mut egui::Ui, state: &mut WebState) {
    let mut select: Option<usize> = None;
    ui.menu_button(
        RichText::new("\u{1F50D}") // 🔍
            .size(CHROME_FONT)
            .color(Style::TEXT_DIM),
        |ui| {
            ui.set_min_width(300.0);
            ui.add(
                egui::TextEdit::singleline(&mut state.tab_search_query)
                    .hint_text("Search tabs")
                    .desired_width(f32::INFINITY),
            );
            ui.separator();
            let matches = matching_tab_indices(&state.tabs, &state.tab_search_query);
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    if matches.is_empty() {
                        ui.weak("No matching tabs");
                    }
                    for idx in matches {
                        let active = idx == state.active;
                        let label = tab_search_row_label(&state.tabs[idx]);
                        let color = if active { Style::TEXT } else { Style::TEXT_DIM };
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(label).size(CHROME_FONT).color(color),
                                )
                                .min_size(egui::vec2(288.0, CHROME_TAB_H)),
                            )
                            .clicked()
                        {
                            select = Some(idx);
                            ui.close_menu();
                        }
                    }
                });
        },
    )
    .response
    .on_hover_text("Search tabs");
    if let Some(idx) = select {
        state.select_tab(idx);
        state.tab_search_query.clear();
    }
}

fn engine_new_tab_buttons(ui: &mut egui::Ui, state: &mut WebState, vertical: bool) {
    let mut button = |ui: &mut egui::Ui, engine: BrowserEngine| {
        let label = format!("+{}", engine.label());
        let mut widget =
            egui::Button::new(RichText::new(label).size(CHROME_FONT).color(Style::TEXT))
                .fill(Style::SURFACE)
                .min_size(egui::vec2(CHROME_NEW_TAB_W, CHROME_TAB_H));
        if vertical {
            widget = widget.min_size(egui::vec2(ui.available_width(), CHROME_TAB_H));
        }
        if ui
            .add(widget)
            .on_hover_text(format!("New {} tab", engine.label()))
            .clicked()
        {
            state.request_new_tab(engine);
        }
    };
    if vertical {
        button(ui, BrowserEngine::Servo);
        button(ui, BrowserEngine::Cef);
    } else {
        button(ui, BrowserEngine::Servo);
        button(ui, BrowserEngine::Cef);
    }
}

/// Which way a tab strip runs, so the shared drag-reorder hit-test knows whether
/// to compare drop points along X (horizontal strip) or Y (vertical strip).
#[derive(Clone, Copy)]
enum TabAxis {
    Horizontal,
    Vertical,
}

/// Distance from a pill rect's centre to `point` along the strip's running axis.
fn tab_axis_distance(rect: egui::Rect, point: egui::Pos2, axis: TabAxis) -> f32 {
    match axis {
        TabAxis::Horizontal => (rect.center().x - point.x).abs(),
        TabAxis::Vertical => (rect.center().y - point.y).abs(),
    }
}

/// Given the laid-out tab pill rects (in tab order) and where a drag was
/// released, return the tab index whose slot the dragged tab should take — the
/// pill whose centre is nearest the drop point along the strip's axis. Reused by
/// both strip variants so horizontal and vertical drag-reorder share one rule.
fn tab_drag_target_index(
    pills: &[(usize, egui::Rect)],
    drop: egui::Pos2,
    axis: TabAxis,
) -> Option<usize> {
    pills
        .iter()
        .min_by(|(_, a), (_, b)| {
            let da = tab_axis_distance(*a, drop, axis);
            let db = tab_axis_distance(*b, drop, axis);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, _)| *idx)
}

/// The width of each pill in the single-row horizontal strip: full width when the
/// strip is roomy, shrinking toward [`CHROME_TAB_MIN_W`] as tabs multiply. Once at
/// the floor the strip scrolls horizontally rather than wrapping onto stacked rows
/// (the industry-standard tab overflow behaviour).
fn horizontal_tab_pill_width(available: f32, tab_count: usize) -> f32 {
    let tab_count = tab_count.max(1) as f32;
    // Each pill also carries its inline close button plus the item spacing on both
    // sides; reserve that so the *visible* pill width is honest and the fit
    // estimate lines up with the real layout.
    let per_slot_overhead = CHROME_TAB_CLOSE + 2.0 * CHROME_GAP;
    let per_tab = available / tab_count - per_slot_overhead;
    per_tab.clamp(CHROME_TAB_MIN_W, CHROME_TAB_W)
}

/// The egui temp-memory key under which the horizontal/vertical strips stash the
/// laid-out pill rects, so egui-driven tests can aim pointer drags at real pill
/// centres (Buttons have no stable id to `read_response`).
#[cfg(test)]
fn tab_pill_rects_id() -> egui::Id {
    egui::Id::new("browser-test-tab-pill-rects")
}

fn tab_pill_sized(ui: &mut egui::Ui, label: &str, active: bool, width: f32) -> egui::Response {
    let color = if active { Style::TEXT } else { Style::TEXT_DIM };
    let fill = if active {
        Style::SURFACE_HI
    } else {
        Style::SURFACE
    };
    // `click_and_drag` so the same pill activates on a plain click, closes on a
    // middle-click, AND reorders on a horizontal/vertical drag. egui's built-in
    // click-vs-drag threshold (`max_click_dist`, 6pt) keeps a click a click.
    ui.add(
        egui::Button::new(RichText::new(label).size(CHROME_FONT).color(color))
            .fill(fill)
            .min_size(egui::vec2(width, CHROME_TAB_H))
            .sense(Sense::click_and_drag()),
    )
}

fn inline_close_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new("\u{00D7}")
                .size(CHROME_FONT)
                .color(Style::TEXT_DIM),
        )
        .fill(Style::SURFACE)
        .min_size(egui::vec2(CHROME_TAB_CLOSE, CHROME_TAB_H)),
    )
    .on_hover_text("Close tab")
}

/// Which speaker glyph (and its hover label) a tab shows, if any: `🔇` when muted
/// (mute WINS — the user hears nothing regardless of playback), `🔊` when audibly
/// playing, `None` when silent and unmuted. Pure, so the choice is unit-tested
/// without a live `Ui`.
fn audio_glyph_for(audible: bool, muted: bool) -> Option<(&'static str, &'static str)> {
    if muted {
        Some(("\u{1F507}", "Unmute tab")) // 🔇
    } else if audible {
        Some(("\u{1F50A}", "Mute tab")) // 🔊
    } else {
        None
    }
}

/// The speaker affordance a tab shows when it is producing audio or is muted —
/// Chrome's audible/mute glyph. Clicking it toggles the tab's mute. Renders (and
/// returns a clickable `Response`) ONLY for an audible or muted tab; a silent,
/// unmuted tab shows nothing so the strip stays quiet.
fn tab_audio_glyph(ui: &mut egui::Ui, audible: bool, muted: bool) -> Option<egui::Response> {
    let (glyph, hover) = audio_glyph_for(audible, muted)?;
    Some(
        ui.add(
            egui::Button::new(
                RichText::new(glyph)
                    .size(CHROME_FONT)
                    .color(Style::TEXT_DIM),
            )
            .fill(Style::SURFACE)
            .min_size(egui::vec2(CHROME_TAB_CLOSE, CHROME_TAB_H)),
        )
        .on_hover_text(hover),
    )
}

fn compact_menu_item(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).size(CHROME_FONT).color(Style::TEXT))
        .min_size(egui::vec2(124.0, CHROME_TAB_H))
}

fn tab_label(tab: &Tab) -> String {
    let title = tab.session.title().trim();
    let url = tab.session.nav().url.trim();
    let base = if !title.is_empty() {
        title
    } else if !url.is_empty() {
        url
    } else {
        "New tab"
    };
    let state = if tab.idle_suspended {
        "\u{25D2}"
    } else {
        match tab.session.state() {
            SessionState::Loading => "\u{25CC}",
            SessionState::Live => "\u{25CF}",
            SessionState::Crashed { .. } => "!",
        }
    };
    let container = tab.container.marker();
    let display = tab.display_target.marker();
    let muted = if tab.muted { "M " } else { "" };
    let force_dark = if tab.force_dark { "D " } else { "" };
    let reader = if tab.reader_mode { "R " } else { "" };
    let user_scripts = if tab.user_scripts { "S " } else { "" };
    let user_agent = tab.user_agent.marker();
    let device_profile = tab.device_profile.marker();
    format!(
        "{state} {container}{display}{muted}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}{}",
        ellipsize(base, 24)
    )
}

fn tab_hover(tab: &Tab) -> String {
    let url = tab.session.nav().url.trim();
    let state = if tab.idle_suspended {
        "Idle suspended"
    } else {
        match tab.session.state() {
            SessionState::Loading => "Loading",
            SessionState::Live => "Live",
            SessionState::Crashed { .. } => "Crashed",
        }
    };
    let container = match tab.container {
        ContainerProfile::None => String::new(),
        profile => format!(" - Container: {}", profile.label()),
    };
    let display = match tab.display_target {
        DisplayTarget::Current => String::new(),
        target => format!(" - Display target: {}", target.label()),
    };
    let audio = if tab.muted { " - Muted" } else { "" };
    let force_dark = if tab.force_dark { " - Force dark" } else { "" };
    let reader = if tab.reader_mode { " - Reader" } else { "" };
    let user_scripts = if tab.user_scripts {
        " - Curated userscripts"
    } else {
        ""
    };
    let user_agent = match tab.user_agent {
        UserAgentOverride::Default => String::new(),
        user_agent => format!(" - User agent: {}", user_agent.label()),
    };
    let device_profile = match tab.device_profile {
        DeviceProfile::Default => String::new(),
        profile => format!(" - Device: {}", profile.label()),
    };
    if url.is_empty() {
        format!(
            "{state}{container}{display}{audio}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
        )
    } else {
        format!(
            "{state} - {url}{container}{display}{audio}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
        )
    }
}

/// Fit a native page-frame size into a thumbnail no wider than `max_w`, preserving
/// aspect ratio; zero for a degenerate (empty) frame. Pure so the fit is unit-tested.
fn thumbnail_size(native: egui::Vec2, max_w: f32) -> egui::Vec2 {
    if native.x <= 0.0 || native.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let w = max_w.min(native.x);
    egui::vec2(w, w * native.y / native.x)
}

/// Tab hover card (industry-grade tab hover): the text summary PLUS, when the tab
/// has a cached page frame, a small thumbnail preview scaled by [`thumbnail_size`].
/// An inactive tab that has not rendered a frame yet just shows the text.
fn tab_hover_card(ui: &mut egui::Ui, tab: &Tab) {
    ui.label(tab_hover(tab));
    if let Some(tex) = &tab.texture {
        let size = thumbnail_size(tex.size_vec2(), 240.0);
        if size.x > 0.0 {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                tex.id(),
                size,
            )));
        }
    }
}

/// A cheap fingerprint of a favicon's PNG bytes, so [`tab_favicon_texture`] can
/// tell "the same favicon as last frame" from "the page just reported a new one"
/// without diffing the byte vector itself on every frame.
fn favicon_fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Resolve this tab's favicon texture for the current frame.
///
/// Reuses [`Tab::favicon_cache`] when the underlying PNG bytes are unchanged from
/// last frame; otherwise PNG-decodes via the same `png`-crate path the boot
/// splash / offline-cache viewport already use ([`crate::chooser::decode_png_rgba`])
/// and caches the result. A decode failure caches an honest `None` rather than
/// panicking or re-attempting the decode every frame (§7).
fn tab_favicon_texture(ctx: &egui::Context, tab: &mut Tab) -> Option<TextureHandle> {
    let bytes = tab.session.favicon()?;
    let fingerprint = favicon_fingerprint(bytes);
    if let Some(cache) = &tab.favicon_cache {
        if cache.fingerprint == fingerprint {
            return cache.texture.clone();
        }
    }
    let texture = crate::chooser::decode_png_rgba(bytes).map(|image| {
        ctx.load_texture(
            format!("browser-tab-favicon::{fingerprint:x}"),
            image,
            TextureOptions::LINEAR,
        )
    });
    tab.favicon_cache = Some(FaviconCache {
        fingerprint,
        texture: texture.clone(),
    });
    texture
}

/// Resolve (and cache) every tab's favicon texture for this frame, in tab order.
///
/// One mutable pass over `tabs` up front, so the tab-strip render loops below —
/// which already borrow each `Tab` by shared reference while building its pill
/// label + context menu — can index into the returned slice instead of fighting
/// this cache for a second `&mut Tab`.
fn resolve_tab_favicon_textures(
    ctx: &egui::Context,
    tabs: &mut [Tab],
) -> Vec<Option<TextureHandle>> {
    tabs.iter_mut()
        .map(|tab| tab_favicon_texture(ctx, tab))
        .collect()
}

/// The favicon slot's fixed size — small enough to sit inline with the pill's
/// [`CHROME_FONT`] label, matching the desktop-browser convention of a page icon
/// immediately left of its tab title.
const TAB_FAVICON_SIZE: f32 = 16.0;

/// Draw one tab's favicon slot immediately before its pill.
///
/// Paints the decoded texture when one resolved this frame; otherwise reserves
/// the same [`TAB_FAVICON_SIZE`] square blank — the pill's own state glyph
/// (`●`/`◌`/`!`, from [`tab_label`]) already carries "no favicon yet", so there is
/// no separate placeholder icon to draw, and reserving the space either way keeps
/// every pill's leading edge aligned regardless of whether its favicon has loaded.
fn tab_favicon_image(ui: &mut egui::Ui, texture: Option<&TextureHandle>) {
    let size = egui::vec2(TAB_FAVICON_SIZE, TAB_FAVICON_SIZE);
    match texture {
        Some(handle) => {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                handle.id(),
                size,
            )));
        }
        None => {
            ui.allocate_space(size);
        }
    }
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i + 1 >= max_chars {
            out.push('\u{2026}');
            return out;
        }
        out.push(ch);
    }
    out
}

/// The crash reason string for a session, or empty if it has not crashed.
fn crash_reason(session: &WebSession) -> String {
    match session.state() {
        SessionState::Crashed { reason } => reason.clone(),
        _ => String::new(),
    }
}

/// Whether a tab should show the TLS/certificate-error interstitial instead
/// of its normal frame — the same precedence `active_body` encodes: crashed
/// wins if a tab is somehow both crashed and cert-blocked, so this is only
/// `true` once `is_crashed` has been ruled out. Pure + testable in isolation
/// from the egui paint path.
fn shows_cert_interstitial(is_crashed: bool, cert_error: Option<&CertError>) -> bool {
    !is_crashed && cert_error.is_some()
}

/// The host to name in the cert-error interstitial, reusing the same
/// dependency-free `host_of` parser the ad filter and third-party checks use
/// (BOOKMARKS-7). Falls back to the raw blocked URL on the rare shape with no
/// `scheme://host` authority, rather than showing a blank domain.
fn cert_error_host(err: &CertError) -> String {
    host_of(&err.url).unwrap_or_else(|| err.url.clone())
}

/// What "Back to safety" does on the cert-error interstitial — go back if the
/// tab has history, otherwise there is nowhere honest to land it but closed
/// (there is no "proceed anyway" past a blocked certificate). Pure decision,
/// factored out of `active_body` so the choice is unit-testable on its own.
enum CertErrorBackAction {
    GoBack,
    CloseTab,
}

fn cert_error_back_action(can_back: bool) -> CertErrorBackAction {
    if can_back {
        CertErrorBackAction::GoBack
    } else {
        CertErrorBackAction::CloseTab
    }
}

// ── BOOKMARKS-10: mesh integration (Send-in-Chat + copy-URL + add-from-page) ──
//
// The Browser surface composes the live page with two mesh services by REUSING
// their existing Bus verbs (§6 JSON boundaries — a local mirror of each topic,
// never a dep on the mackesd worker, never a re-derived store):
//
//   * add-from-page → `action/bookmarks/add` — the BOOKMARKS-2 worker (the
//                      single writer of this node's op-log segment + HLC clock,
//                      the services/desktop tier boundary) drains it, mints the
//                      real `Op::Add`, persists it, and Syncthing-syncs it across
//                      the mesh. `source` is omitted → the honest `Source::Manual`.
//   * Send-in-Chat  → `action/chat/send` — the SAME verb the shell's Chat composer
//                      and Files' `chat_bridge` publish; a link rides the
//                      NOTIFY-CHAT `Clipboard` message-kind (a `kind` wins over
//                      `text` in the worker), addressed to this node's own Chat
//                      contact (the notification hub where its clips land).
//   * copy-URL      → the shell clipboard — egui's output command (the DRM /
//                      windowed backend owns the wire), the same one-click re-copy
//                      the Chat clipboard card uses.

/// The mackesd bookmarks worker's add verb (`action/bookmarks/<verb>`, §9).
const ACTION_BOOKMARKS_ADD: &str = "action/bookmarks/add";

/// The mackesd chat worker's send verb (reused, never re-invented).
const ACTION_CHAT_SEND: &str = "action/chat/send";

/// The mackesd adfilter worker's mesh-wide per-site allowlist verbs.
const ACTION_ADFILTER_ALLOW: &str = "action/adfilter/allow";
const ACTION_ADFILTER_BLOCK: &str = "action/adfilter/block";

/// Browser-to-platform display placement handoff. The shell owns tab intent; the
/// compositor/display owner drains this stream when it can perform the output
/// migration.
const ACTION_BROWSER_DISPLAY_TARGET: &str = "action/browser/display-target";

/// Browser external-protocol handoff for schemes without a dedicated worker yet.
const ACTION_BROWSER_PROTOCOL: &str = "action/browser/protocol";

/// Browser page-share handoff for platform targets whose receivers live outside
/// the Browser surface.
const ACTION_BROWSER_SHARE: &str = "action/browser/share";

/// Browser follow-me/send-tab handoff. The session-sync owner drains this stream
/// and delivers the live tab to a mesh node or a paired phone; Browser only owns
/// the tab metadata and stable user action.
const ACTION_BROWSER_SEND_TAB: &str = "action/browser/send-tab";

/// Browser passkey/WebAuthn ceremony handoff. The helper observes page WebAuthn
/// calls; Browser adds local source metadata; the daemon passkey owner validates
/// and persists the pending ceremony.
const ACTION_BROWSER_PASSKEY: &str = "action/browser/passkey";

/// Browser read-aloud handoff. The Browser owns page text extraction; the TTS
/// owner drains this bounded text request and performs speech synthesis/playback.
const ACTION_BROWSER_READ_ALOUD: &str = "action/browser/read-aloud";

/// Browser translation handoff. The Browser owns page text extraction; the
/// offline/mesh translation owner drains this bounded private request.
const ACTION_BROWSER_TRANSLATE: &str = "action/browser/translate";

/// Browser offline-cache handoff. The Browser owns page text extraction; the
/// cache owner persists a private local copy and mirrors it onto the mesh file
/// plane without re-enabling helper disk cache.
const ACTION_BROWSER_OFFLINE_CACHE: &str = "action/browser/offline-cache";

/// Browser voice-command/dictation handoff. The Browser owns active-tab context;
/// the STT owner drains this request, captures audio, and publishes/apply commands.
const ACTION_BROWSER_VOICE_COMMAND: &str = "action/browser/voice-command";

/// Browser prompted sensitive-device permission decision. The current helpers
/// enforce deny-all; this stream records the prompt decision and gives the later
/// engine permission hook a typed contract.
const ACTION_BROWSER_PERMISSION_PROMPT: &str = "action/browser/permission-prompt";

/// Browser voice-command transcript result prefix, owned by the daemon STT worker.
const EVENT_BROWSER_VOICE_COMMAND_PREFIX: &str = "event/browser-voice-command/";

/// Browser read-aloud status prefix, owned by the daemon TTS worker.
const STATE_BROWSER_READ_ALOUD_PREFIX: &str = "state/browser-read-aloud/";

/// Browser voice-command status prefix, owned by the daemon STT worker.
const STATE_BROWSER_VOICE_COMMAND_PREFIX: &str = "state/browser-voice-command/";

/// Browser passkey/WebAuthn status prefix, owned by the daemon passkey worker.
const STATE_BROWSER_PASSKEYS_PREFIX: &str = "state/browser-passkeys/";

/// Browser passkey/WebAuthn completion-event prefix, owned by the daemon worker.
const EVENT_BROWSER_PASSKEYS_PREFIX: &str = "event/browser-passkeys/";

/// Browser translation result prefix, owned by the daemon translation worker.
const EVENT_BROWSER_TRANSLATE_PREFIX: &str = "event/browser-translate/";

/// Browser platform-share route event prefix, owned by the daemon share worker.
const EVENT_BROWSER_SHARE_PREFIX: &str = "event/browser-share/";

/// Browser offline-cache record prefix, owned by the daemon cache worker.
const EVENT_BROWSER_OFFLINE_CACHE_PREFIX: &str = "event/browser-offline-cache/";

/// Browser CEF security-update status prefix, owned by the daemon updater worker.
const STATE_BROWSER_SECURITY_UPDATE_PREFIX: &str = "state/browser-security-update/";

/// Browser follow-me session snapshot. The sync owner drains this stream into the
/// Nebula+Syncthing session store and later drives startup restore; Browser only
/// publishes the state it already owns.
const ACTION_BROWSER_SESSION_SYNC: &str = "action/browser/session-sync";

/// Daemon-owned Browser session-sync snapshot subdirectory. Must match
/// `mackesd::workers::browser_session_sync::SESSION_SYNC_SUBDIR` without creating
/// a desktop-shell dependency on the daemon crate.
const SESSION_SYNC_SUBDIR: &str = "browser-session-sync";

/// Daemon-owned latest snapshot filename. The file body is the Browser snapshot
/// JSON itself, so startup restore can feed it straight into the parser.
const SESSION_SYNC_LATEST_FILE: &str = "latest.json";

/// Daemon-owned send-tab outbox subdirectory. Must match
/// `mackesd::workers::browser_session_sync::SEND_TAB_OUTBOX_SUBDIR`.
const SEND_TAB_OUTBOX_SUBDIR: &str = "browser-send-tab";

/// Browser idle-tab suspension handoff for deeper engine/process orchestration.
const ACTION_BROWSER_TAB_SUSPEND: &str = "action/browser/tab-suspend";

/// Existing Voice handoff used by Chat's Call action; Browser `tel:` URLs reuse
/// it instead of creating a second dial verb.
const ACTION_VOICE_DIAL: &str = "action/voice/dial";

/// Browser-originated notifications folded by Chat's alert lane into the unified
/// Notifications feed.
const EVENT_NOTIFY_BROWSER: &str = "event/notify/browser";

/// The mesh-hosted SearXNG endpoint used by the first-cut omnibox when the draft
/// is plain search text rather than a URL. This is intentionally mesh-local, not a
/// public search provider default.
const DEFAULT_SEARCH_URL: &str = "https://search.mesh/search";

/// A user-selectable search engine (Tier-2 "configurable search engines"): a display
/// `name`, an omnibox `keyword` shortcut (type "`kw` query" to use it — Chrome's
/// per-keyword search / tab-to-search), and a `template` whose `%s` is replaced by
/// the percent-encoded query. Session-only for now; the model is the extension point
/// for an operator-managed list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchEngine {
    name: String,
    keyword: String,
    template: String,
}

/// The default engine set: the mesh SearXNG search plus its image/video category
/// shortcuts (SearXNG honors `&categories=`). The first entry is the fallback default
/// when no keyword matches; the rest are keyword shortcuts. Mesh-local by design (no
/// public provider default), matching [`DEFAULT_SEARCH_URL`].
fn default_search_engines() -> Vec<SearchEngine> {
    vec![
        SearchEngine {
            name: "Mesh Search".to_owned(),
            keyword: "s".to_owned(),
            template: format!("{DEFAULT_SEARCH_URL}?q=%s"),
        },
        SearchEngine {
            name: "Mesh Images".to_owned(),
            keyword: "img".to_owned(),
            template: format!("{DEFAULT_SEARCH_URL}?categories=images&q=%s"),
        },
        SearchEngine {
            name: "Mesh Videos".to_owned(),
            keyword: "vid".to_owned(),
            template: format!("{DEFAULT_SEARCH_URL}?categories=videos&q=%s"),
        },
    ]
}

/// If `draft` begins with a configured engine's keyword followed by a query
/// ("`img` sunset"), return that engine's URL with `%s` replaced by the
/// percent-encoded query; else `None` (the caller falls back to the default router).
/// Pure + unit-tested.
fn keyword_search_target(draft: &str, engines: &[SearchEngine]) -> Option<String> {
    let (kw, rest) = draft.trim().split_once(char::is_whitespace)?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let engine = engines
        .iter()
        .find(|e| e.keyword.eq_ignore_ascii_case(kw))?;
    Some(engine.template.replace("%s", &percent_encode_query(rest)))
}

/// Mesh-local SearXNG autocomplete endpoint. SearXNG deployments commonly expose
/// autocomplete through this path; the parser below accepts the JSON shapes used
/// by SearXNG/OpenSearch-style providers so the shell does not bake in one brittle
/// response variant.
const DEFAULT_SUGGEST_URL: &str = "https://search.mesh/autocompleter";

/// Keep a live suggestion request tight; this is interactive chrome, not page load.
const SUGGEST_TIMEOUT: Duration = Duration::from_millis(900);

type SuggestionResult = Result<(String, Vec<String>), String>;
type SpellcheckResult = (u64, Result<Vec<SpellMiss>, String>);

mod content_tools;
use content_tools::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceCommandMode {
    Command,
    Dictation,
}

impl VoiceCommandMode {
    fn wire(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Dictation => "dictation",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Command => "Voice command",
            Self::Dictation => "Dictation",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "command" => Some(Self::Command),
            "dictation" => Some(Self::Dictation),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VoiceTranscriptResult {
    host: String,
    mode: VoiceCommandMode,
    tab_index: usize,
    focus: String,
    transcript: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserReadAloudStatus {
    node: String,
    last_title: Option<String>,
    last_url: Option<String>,
    state: String,
    last_error: Option<String>,
    accepted: u64,
    spoken: u64,
    rejected: u64,
    last_request_ms: Option<u64>,
    updated_ms: u64,
}

impl BrowserReadAloudStatus {
    fn is_visible(&self) -> bool {
        self.state != "idle" || self.accepted > 0 || self.rejected > 0
    }

    fn is_actionable(&self) -> bool {
        matches!(self.state.as_str(), "speaking" | "unavailable" | "error")
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "spoken" => ChipTone::Ok,
            "speaking" => ChipTone::Info,
            "unavailable" | "error" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn chip_label(&self) -> String {
        format!("TTS {}", self.state)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserVoiceCommandStatus {
    node: String,
    last_url: Option<String>,
    last_mode: Option<String>,
    state: String,
    last_error: Option<String>,
    accepted: u64,
    transcribed: u64,
    rejected: u64,
    last_transcript_chars: Option<u64>,
    last_request_ms: Option<u64>,
    updated_ms: u64,
}

impl BrowserVoiceCommandStatus {
    fn is_visible(&self) -> bool {
        self.state != "idle" || self.accepted > 0 || self.rejected > 0
    }

    fn is_actionable(&self) -> bool {
        matches!(self.state.as_str(), "listening" | "unavailable" | "error")
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "transcribed" => ChipTone::Ok,
            "listening" => ChipTone::Info,
            "unavailable" | "error" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn chip_label(&self) -> String {
        match self.last_mode.as_deref() {
            Some("dictation") => format!("Dictation {}", self.state),
            _ => format!("Voice {}", self.state),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserPasskeyStatus {
    node: String,
    last_request_id: Option<String>,
    last_host: Option<String>,
    last_ceremony: Option<String>,
    last_rp_id: Option<String>,
    state: String,
    mirrored: bool,
    last_error: Option<String>,
    accepted: u64,
    rejected: u64,
    last_pending_ms: Option<u64>,
    hardware_state: String,
    hardware_key_count: u64,
    hardware_readable_count: u64,
    hardware_ctaphid_state: String,
    hardware_ctaphid_init_frame_count: u64,
    hardware_probe_ms: u64,
    updated_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserPasskeyCompletion {
    client_request_id: String,
    body: String,
}

impl BrowserPasskeyStatus {
    fn ceremony_is_visible(&self) -> bool {
        self.state != "idle" || self.accepted > 0 || self.rejected > 0
    }

    fn hardware_is_visible(&self) -> bool {
        self.hardware_state != "unknown"
    }

    fn ctaphid_is_visible(&self) -> bool {
        self.hardware_ctaphid_state == "init_request_ready"
            && self.hardware_ctaphid_init_frame_count > 0
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "pending" => ChipTone::Info,
            "created" | "asserted" => ChipTone::Ok,
            "error" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn chip_label(&self) -> String {
        match self.state.as_str() {
            "pending" => "Passkey pending".to_owned(),
            "created" => "Passkey created".to_owned(),
            "asserted" => "Passkey asserted".to_owned(),
            "error" => "Passkey error".to_owned(),
            other => format!("Passkey {other}"),
        }
    }

    fn hardware_tone(&self) -> ChipTone {
        match self.hardware_state.as_str() {
            "ready" => ChipTone::Ok,
            "present_permission_denied" => ChipTone::Warn,
            "unavailable" => ChipTone::Neutral,
            _ => ChipTone::Neutral,
        }
    }

    fn hardware_chip_label(&self) -> String {
        match self.hardware_state.as_str() {
            "ready" => "Security key ready".to_owned(),
            "present_permission_denied" => "Security key blocked".to_owned(),
            "unavailable" => "Security key unavailable".to_owned(),
            other => format!("Security key {other}"),
        }
    }

    fn ctaphid_tone(&self) -> ChipTone {
        match self.hardware_ctaphid_state.as_str() {
            "init_request_ready" => ChipTone::Info,
            _ => ChipTone::Neutral,
        }
    }

    fn ctaphid_chip_label(&self) -> String {
        match self.hardware_ctaphid_state.as_str() {
            "init_request_ready" => "CTAP INIT framed".to_owned(),
            other => format!("CTAP HID {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserTranslationResult {
    host: String,
    tab_index: usize,
    engine: BrowserEngine,
    url: String,
    title: String,
    source_lang: String,
    target_lang: String,
    translation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserShareRouteResult {
    host: String,
    target: BrowserShareTarget,
    url: String,
    title: String,
    preview: String,
    request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserQrShareResult {
    host: String,
    url: String,
    title: String,
    preview: String,
    request_id: String,
    modules: Vec<Vec<bool>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserOfflineCacheResult {
    host: String,
    cache_id: String,
    tab_index: usize,
    engine: BrowserEngine,
    url: String,
    title: String,
    text: String,
    viewport: Option<OfflineCacheViewportImage>,
    resources: Vec<OfflineCacheResource>,
    archive_mhtml: Option<OfflineCacheArchive>,
    pdf_snapshot: Option<OfflineCachePdf>,
    cached_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserSecurityUpdateStatus {
    node: String,
    state: String,
    expected_cef_version: Option<String>,
    expected_chromium_version: Option<String>,
    expected_channel: Option<String>,
    active_runtime: Option<String>,
    installed_version: Option<String>,
    installed_chromium: Option<String>,
    libcef_present: bool,
    updater_state: String,
    last_update_ms: Option<u64>,
    last_update_exit_code: Option<i32>,
    last_update_error: Option<String>,
    last_error: Option<String>,
    updated_ms: u64,
}

impl BrowserSecurityUpdateStatus {
    fn is_actionable(&self) -> bool {
        self.state != "current" || !matches!(self.updater_state.as_str(), "idle" | "attempted")
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "current" if matches!(self.updater_state.as_str(), "idle" | "attempted") => {
                ChipTone::Ok
            }
            "missing" | "mismatch" | "manifest_missing" => ChipTone::Warn,
            _ if self.updater_state == "installing" => ChipTone::Info,
            _ if self.updater_state == "failed" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn chip_label(&self) -> String {
        match self.state.as_str() {
            "current" => "CEF current".to_owned(),
            "missing" => "CEF missing".to_owned(),
            "mismatch" => "CEF mismatch".to_owned(),
            "manifest_missing" => "CEF manifest".to_owned(),
            other => format!("CEF {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserVoiceAction {
    NewTab,
    CloseTab,
    Back,
    Forward,
    Reload,
    ReadAloud,
    Find(String),
}

#[derive(Default)]
struct SpellcheckState {
    in_flight: Option<u64>,
    tab_index: Option<usize>,
    rx: Option<mpsc::Receiver<SpellcheckResult>>,
}

impl SpellcheckState {
    fn poll(&mut self) -> Option<(u64, usize, Result<Vec<SpellMiss>, String>)> {
        let rx = self.rx.take()?;
        match rx.try_recv() {
            Ok((id, result)) => {
                self.in_flight = None;
                let tab_index = self.tab_index.take().unwrap_or_default();
                Some((id, tab_index, result))
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.rx = Some(rx);
                None
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let id = self.in_flight.take().unwrap_or_default();
                let tab_index = self.tab_index.take().unwrap_or_default();
                Some((id, tab_index, Err("Spell-check unavailable".to_owned())))
            }
        }
    }

    fn start(&mut self, id: u64, tab_index: usize, text: String) {
        let (tx, rx) = mpsc::channel();
        self.in_flight = Some(id);
        self.tab_index = Some(tab_index);
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = spell::run_hunspell(spell::HUNSPELL, &text)
                .map_err(|state| state.notice().to_owned());
            let _ = tx.send((id, result));
        });
    }
}

fn spellcheck_notice(result: Result<Vec<SpellMiss>, String>) -> String {
    match result {
        Ok(misses) if misses.is_empty() => "Spelling: no misspellings found".to_owned(),
        Ok(misses) => {
            let count = misses.len();
            let plural = if count == 1 { "" } else { "s" };
            let first = misses
                .first()
                .map(|miss| {
                    let suggestion = miss
                        .suggestions
                        .first()
                        .map_or(String::new(), |s| format!(" -> {s}"));
                    format!("; first: {}{}", miss.word, suggestion)
                })
                .unwrap_or_default();
            format!("Spelling: {count} possible misspelling{plural}{first}")
        }
        Err(err) if err.is_empty() => "Spelling unavailable".to_owned(),
        Err(err) => format!("Spelling unavailable: {err}"),
    }
}

fn spellcheck_highlight_words(misses: &[SpellMiss]) -> Vec<String> {
    let mut words = BTreeSet::new();
    for miss in misses {
        let word = miss.word.trim();
        if word.len() < 2 || word.len() > 64 {
            continue;
        }
        words.insert(word.to_owned());
        if words.len() >= 64 {
            break;
        }
    }
    words.into_iter().collect()
}

fn spellcheck_occurrence_index(misses: &[SpellMiss], row_index: usize) -> u16 {
    let Some(current) = misses.get(row_index) else {
        return 0;
    };
    let word = current.word.trim();
    if word.is_empty() {
        return 0;
    }
    let prior = misses
        .iter()
        .take(row_index)
        .filter(|miss| miss.word.trim().eq_ignore_ascii_case(word))
        .count();
    prior.min(u16::MAX as usize) as u16
}

#[derive(Default)]
struct SuggestionState {
    draft: String,
    items: Vec<String>,
    notice: Option<String>,
    in_flight: Option<String>,
    rx: Option<mpsc::Receiver<SuggestionResult>>,
    /// Matching browsing-history visit URLs for the current draft — most-recent-first,
    /// capped (see [`WebState::update_suggestions_for_address`]). Rendered ABOVE the
    /// SearXNG `items` in the omnibox dropdown (Chrome-style). Set independently of the
    /// search-suggestion fetch gate ([`should_fetch_suggestions`]), so a URL-like draft
    /// that skips the SearXNG round-trip still surfaces a matching visit. Session-only —
    /// mirrors [`HistoryStore`], never persisted.
    history: Vec<String>,
    /// Matching bookmarks (`{title, url}`) for the current draft — rendered ABOVE
    /// history in the dropdown (Chrome ranks a saved bookmark above a mere visit).
    bookmarks: Vec<BookmarkBarLink>,
    /// Keyboard-highlighted suggestion — a flat index into
    /// [`Self::ordered_commit_values`] (bookmarks, then history, then search).
    /// `None` = nothing highlighted (Enter submits the typed draft). Reset whenever
    /// the draft changes; moved by Up/Down while the omnibox has focus.
    selected: Option<usize>,
}

/// Next keyboard-highlight index after moving `delta` (±1) over `len` suggestions,
/// wrapping at both ends; from nothing highlighted, Down picks the first and Up the
/// last (Chrome's omnibox behavior). Pure so the traversal is unit-tested directly.
fn next_selection(current: Option<usize>, delta: i32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None => {
            if delta > 0 {
                0
            } else {
                len - 1
            }
        }
        Some(cur) => (cur as i32 + delta).rem_euclid(len as i32) as usize,
    })
}

/// The index to preselect for Chrome's "inline top-hit": `Some(0)` when the first
/// suggestion is an inline completion of the draft (the trimmed draft is a
/// case-insensitive prefix of it AND the suggestion adds more), so Enter accepts the
/// completed URL. `None` otherwise (nothing preselected; arrows drive selection).
/// Pure so the preselect rule is unit-tested directly.
fn inline_top_hit(ordered: &[String], draft: &str) -> Option<usize> {
    let d = draft.trim().to_lowercase();
    if d.is_empty() {
        return None;
    }
    let top = ordered.first()?;
    (top.to_lowercase().starts_with(&d) && top.trim().len() > d.len()).then_some(0)
}

impl SuggestionState {
    fn clear(&mut self) {
        self.draft.clear();
        self.items.clear();
        self.notice = None;
        self.in_flight = None;
        self.rx = None;
        self.history.clear();
        self.bookmarks.clear();
        self.selected = None;
    }

    /// Replace the history-match list (see [`SuggestionState::history`]).
    fn set_history_matches(&mut self, matches: Vec<String>) {
        self.history = matches;
    }

    /// Replace the bookmark-match list (see [`SuggestionState::bookmarks`]).
    fn set_bookmark_matches(&mut self, matches: Vec<BookmarkBarLink>) {
        self.bookmarks = matches;
    }

    /// The flat suggestion list in RENDER order (bookmarks, history, deduped search)
    /// as the strings that get committed on Enter — the index space for [`Self::selected`].
    fn ordered_commit_values(&self) -> Vec<String> {
        let mut v: Vec<String> = self.bookmarks.iter().map(|b| b.url.clone()).collect();
        v.extend(self.history.iter().cloned());
        v.extend(
            dedup_search_items(&self.items, &self.history)
                .into_iter()
                .cloned(),
        );
        v
    }

    /// Move the keyboard highlight by `delta` (±1), wrapping over the current list.
    fn move_selection(&mut self, delta: i32) {
        let len = self.ordered_commit_values().len();
        self.selected = next_selection(self.selected, delta, len);
    }

    /// The commit value under the keyboard highlight, if any (Enter accepts it
    /// instead of the typed draft).
    fn selected_value(&self) -> Option<String> {
        self.selected
            .and_then(|i| self.ordered_commit_values().into_iter().nth(i))
    }

    fn poll(&mut self) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((draft, items))) => {
                if draft == self.draft {
                    self.items = items;
                    self.notice = None;
                }
                self.in_flight = None;
            }
            Ok(Err(err)) => {
                self.items.clear();
                self.notice = Some(err);
                self.in_flight = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.rx = Some(rx);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.items.clear();
                self.notice = Some("Suggestions unavailable".to_owned());
                self.in_flight = None;
            }
        }
    }

    fn update_for_draft(&mut self, draft: &str) {
        self.poll();
        let draft = draft.trim();
        if !should_fetch_suggestions(draft) {
            self.clear();
            return;
        }
        if self.draft != draft {
            self.draft = draft.to_owned();
            self.items.clear();
            self.notice = None;
        }
        if self.in_flight.as_deref() == Some(draft) {
            return;
        }
        self.in_flight = Some(draft.to_owned());
        let request = draft.to_owned();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = fetch_suggestions(&request).map(|items| (request.clone(), items));
            let _ = tx.send(result);
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MeshServiceShortcut {
    label: &'static str,
    url: &'static str,
    hint: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpeedDialEntry {
    label: String,
    url: String,
    hint: String,
}

impl SpeedDialEntry {
    fn new(label: impl Into<String>, url: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            url: url.into(),
            hint: hint.into(),
        }
    }
}

const NEW_TAB_SERVICES: [MeshServiceShortcut; 4] = [
    MeshServiceShortcut {
        label: "Search",
        url: "https://search.mesh/",
        hint: "Open the mesh SearXNG front page",
    },
    MeshServiceShortcut {
        label: "Music",
        url: "http://music.mesh:4533/",
        hint: "Open the active-active Navidrome mesh service",
    },
    MeshServiceShortcut {
        label: "Horizon",
        url: "https://horizon.mesh/",
        hint: "Open the optional OpenStack dashboard when enabled",
    },
    MeshServiceShortcut {
        label: "Keystone",
        url: "http://keystone.mesh:5000/v3",
        hint: "Open the OpenStack identity API endpoint",
    },
];

fn default_speed_dial() -> Vec<SpeedDialEntry> {
    NEW_TAB_SERVICES
        .iter()
        .map(|service| SpeedDialEntry::new(service.label, service.url, service.hint))
        .collect()
}

fn default_session_restore_roots() -> Vec<PathBuf> {
    vec![local_session_sync_root(), default_workgroup_root()]
}

fn local_session_sync_root() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map_or_else(
        || {
            std::env::var_os("HOME").map_or_else(
                || PathBuf::from("/var/lib/mde/browser-session-sync"),
                |home| {
                    PathBuf::from(home)
                        .join(".local")
                        .join("share")
                        .join("mde")
                        .join("browser-session-sync")
                },
            )
        },
        |data_home| {
            PathBuf::from(data_home)
                .join("mde")
                .join("browser-session-sync")
        },
    )
}

fn session_sync_latest_path(root: &Path, host: &str) -> PathBuf {
    root.join(SESSION_SYNC_SUBDIR)
        .join(sanitize_session_host(host))
        .join(SESSION_SYNC_LATEST_FILE)
}

fn send_tab_inbox_dir(root: &Path, host: &str) -> PathBuf {
    root.join(SEND_TAB_OUTBOX_SUBDIR)
        .join("node")
        .join(sanitize_session_host(host))
}

fn sanitize_session_host(host: &str) -> String {
    host.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                Some(c)
            } else if c.is_ascii_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn browser_send_tab_open_intent(body: &str, host: &str) -> Result<(BrowserEngine, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("send-tab JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_send_tab") {
        return Err("send-tab has the wrong op".to_owned());
    }
    if v.get("target").and_then(serde_json::Value::as_str) != Some("node") {
        return Err("send-tab is not node-addressed".to_owned());
    }
    let target_id = v
        .get("target_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|target_id| !target_id.is_empty())
        .ok_or_else(|| "send-tab is missing target_id".to_owned())?;
    if sanitize_session_host(target_id) != sanitize_session_host(host) {
        return Err("send-tab is for a different node".to_owned());
    }
    let engine = v
        .get("engine")
        .and_then(serde_json::Value::as_str)
        .and_then(BrowserEngine::from_wire)
        .ok_or_else(|| "send-tab has an unsupported engine".to_owned())?;
    let url = v
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "send-tab is missing url".to_owned())?;
    Ok((engine, url.to_owned()))
}

fn incoming_send_tab_files(root: &Path, host: &str) -> Vec<PathBuf> {
    let inbox = send_tab_inbox_dir(root, host);
    let Ok(sources) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for source in sources.filter_map(Result::ok) {
        let source_path = source.path();
        if !source_path.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&source_path) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn speed_dial_from_settings(settings: &serde_json::Value) -> Option<Vec<SpeedDialEntry>> {
    let entries = settings.get("speed_dial")?.as_array()?;
    let restored = entries
        .iter()
        .filter_map(|entry| {
            let label = entry
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            let url = entry
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            if label.is_empty() || url.is_empty() {
                return None;
            }
            let hint = entry
                .get("hint")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|hint| !hint.is_empty())
                .unwrap_or(url);
            Some(SpeedDialEntry::new(label, url, hint))
        })
        .take(32)
        .collect::<Vec<_>>();
    (!restored.is_empty()).then_some(restored)
}

mod wire;
use wire::*;

/// Mint the transfer job a completed browser download or scraper output uses once
/// the helper has materialized the file locally. The browser does not crawl or move
/// bytes itself here; it hands each resulting file to the daemon-owned Transfers
/// queue, preserving one download manager and one ledger (TRANSFERS-10).
fn browser_output_transfer_job(source: &str, dest: &str) -> TransferJob {
    TransferJob::new(
        source.trim(),
        dest.trim(),
        TransferMethod::BrowserDownload,
        TransferPolicy {
            bwlimit: None,
            verify: true,
        },
    )
}

/// Enqueue one materialized browser output into the shared Transfers queue.
///
/// # Errors
/// Returns an honest validation/dispatch error when either path is empty or the
/// transfer client cannot write the daemon inbox.
fn enqueue_browser_output(
    transfers: &dyn TransfersClient,
    source: &str,
    dest: &str,
) -> Result<String, String> {
    if source.trim().is_empty() || dest.trim().is_empty() {
        return Err("browser transfer enqueue requires source and destination paths".into());
    }
    let job = browser_output_transfer_job(source, dest);
    let id = job.id.clone();
    transfers.dispatch(&TransferVerb::Submit(job))?;
    Ok(id)
}

/// Enqueue every file produced by a Power-mode scrape/export batch. Each file
/// becomes its own `browser_download` job, so progress, verify, notify, pause, and
/// history stay in the same Transfers surface as ordinary downloads.
///
/// # Errors
/// Returns the first validation/dispatch error; any ids returned before that point
/// were already handed to the transfer worker.
fn enqueue_browser_output_batch(
    transfers: &dyn TransfersClient,
    sources: &[String],
    dest: &str,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(sources.len());
    for source in sources {
        ids.push(enqueue_browser_output(transfers, source, dest)?);
    }
    Ok(ids)
}

/// Publish `body` on `topic` via the persist-first path (the same discipline as
/// the shell's Chat composer + Files' `chat_bridge`). Best-effort: no Bus on this
/// node / a transient open failure is a silent no-op — the honest solo-host
/// state, never a panic.
fn publish(topic: &str, body: &str) {
    publish_to_bus(mde_bus::client_data_dir().as_deref(), topic, body);
}

fn publish_to_bus(root: Option<&Path>, topic: &str, body: &str) {
    let Some(root) = root else { return };
    let Ok(persist) = Persist::open(root.to_path_buf()) else {
        return;
    };
    let _ = persist.write(topic, Priority::Default, None, Some(body));
}

/// The local hostname — the mesh identity a Send-in-Chat addresses (lock 2/21:
/// the hostname *is* the chat contact username). `$HOSTNAME` →
/// `/proc/sys/kernel/hostname` → `/etc/hostname` → `"localhost"`.
fn local_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(h) = std::fs::read_to_string(path) {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    "localhost".to_string()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

mod capture;
use capture::*;
mod drawers;
use drawers::*;

/// The Browser page-actions menu (BOOKMARKS-10): the three mesh-integration verbs
/// on the current page. Rendered by BOTH the toolbar menu button and the address
/// bar's right-click context menu (one body, two entry points). Each item greys
/// out with no live URL to act on. §4 Carbon tokens on the chrome.
fn page_actions_menu(
    ui: &mut egui::Ui,
    bus_root: Option<&Path>,
    engine: Option<BrowserEngine>,
    url: &str,
    title: &str,
) {
    let has_page = !url.trim().is_empty();
    // Add-from-page → the mesh-synced bookmarks store (via the worker's add verb).
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(RichText::new("\u{2606}  Add bookmark").color(Style::TEXT)),
        )
        .clicked()
    {
        publish(ACTION_BOOKMARKS_ADD, &bookmark_add_body(url, title));
        ui.close_menu();
    }
    // Copy-URL → the shell clipboard (egui's output command).
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(RichText::new("\u{29C9}  Copy URL").color(Style::TEXT)),
        )
        .clicked()
    {
        ui.ctx().copy_text(url.to_string());
        ui.close_menu();
    }
    // Send-in-Chat → the NOTIFY-CHAT send verb.
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(RichText::new("\u{1F4AC}  Send in Chat").color(Style::TEXT)),
        )
        .clicked()
    {
        publish(
            ACTION_CHAT_SEND,
            &chat_share_body(&local_hostname(), url, title),
        );
        ui.close_menu();
    }
    for target in [
        BrowserShareTarget::Peer,
        BrowserShareTarget::Phone,
        BrowserShareTarget::Email,
        BrowserShareTarget::Qr,
    ] {
        if ui
            .add_enabled(
                has_page,
                egui::Button::new(
                    RichText::new(format!("{}  Share to {}", "\u{21AA}", target.label()))
                        .color(Style::TEXT),
                ),
            )
            .clicked()
        {
            publish_browser_share(bus_root, target, url, title);
            ui.close_menu();
        }
    }
    for target in [BrowserSendTabTarget::Node, BrowserSendTabTarget::Phone] {
        if ui
            .add_enabled(
                has_page,
                egui::Button::new(
                    RichText::new(format!("{}  Send tab to {}", "\u{21E5}", target.label()))
                        .color(Style::TEXT),
                ),
            )
            .clicked()
        {
            if let Some(engine) = engine {
                publish_browser_send_tab(bus_root, target, engine, url, title);
            }
            ui.close_menu();
        }
    }
}

/// The toolbar 🔑 password menu: fill a saved login into the current site's form, or
/// save a new credential for it. Session-only store, user-initiated fill (the engine
/// injects the fill script). Deferred actions are applied after the popup closure so
/// the store borrow ends before the save-form's mutable draft edit.
fn password_menu(ui: &mut egui::Ui, state: &mut WebState, page_url: &str, has_page: bool) {
    let host = host_of(page_url).unwrap_or_default();
    let mut fill: Option<(String, String)> = None;
    let mut remove: Option<usize> = None;
    let mut save = false;
    // Clone matches out (index, user, pass) so the store borrow does not outlive into
    // the save-form's `&mut login_*_draft`.
    let matches: Vec<(usize, String, String)> = if host.is_empty() {
        Vec::new()
    } else {
        state
            .session_logins
            .iter()
            .enumerate()
            .filter(|(_, l)| l.host == host)
            .map(|(i, l)| (i, l.username.clone(), l.password.clone()))
            .collect()
    };
    ui.menu_button(
        RichText::new("\u{1F511}") // 🔑
            .size(CHROME_FONT)
            .color(Style::TEXT_DIM),
        |ui| {
            ui.set_min_width(260.0);
            if host.is_empty() {
                ui.weak("No site loaded");
                return;
            }
            ui.label(
                RichText::new("Saved logins (this session)")
                    .size(CHROME_FONT)
                    .strong(),
            );
            if matches.is_empty() {
                ui.weak(format!("None saved for {host}"));
            } else {
                for (idx, username, password) in &matches {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                has_page,
                                egui::Button::new(
                                    RichText::new(format!("Fill {username}")).size(CHROME_FONT),
                                ),
                            )
                            .clicked()
                        {
                            fill = Some((username.clone(), password.clone()));
                            ui.close_menu();
                        }
                        if ui
                            .add(egui::Button::new(
                                RichText::new("\u{00D7}").size(CHROME_FONT),
                            ))
                            .on_hover_text("Delete saved login")
                            .clicked()
                        {
                            remove = Some(*idx);
                            ui.close_menu();
                        }
                    });
                }
            }
            ui.separator();
            ui.label(RichText::new(format!("Save a login for {host}")).size(CHROME_FONT));
            ui.add(
                egui::TextEdit::singleline(&mut state.login_user_draft)
                    .hint_text("username")
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::TextEdit::singleline(&mut state.login_pass_draft)
                    .password(true)
                    .hint_text("password")
                    .desired_width(f32::INFINITY),
            );
            if ui
                .add(egui::Button::new(RichText::new("Save").size(CHROME_FONT)))
                .clicked()
            {
                save = true;
                ui.close_menu();
            }
        },
    );
    if let Some((user, pass)) = fill {
        state.fill_active_login(user, pass);
    }
    if let Some(idx) = remove {
        state.remove_login(idx);
    }
    if save {
        let user = std::mem::take(&mut state.login_user_draft);
        let pass = std::mem::take(&mut state.login_pass_draft);
        state.save_login(&host, &user, &pass);
    }
}

/// The toolbar star that opens the BOOKMARKS-10 [`page_actions_menu`]; the glyph
/// dims with no live page (the menu items disable themselves too). Split out of
/// [`nav_chrome`] to keep that toolbar within its line budget.
fn page_actions_button(
    ui: &mut egui::Ui,
    has_page: bool,
    is_bookmarked: bool,
    bus_root: Option<&Path>,
    engine: Option<BrowserEngine>,
    url: &str,
    title: &str,
) {
    // Filled accent star when the current page is bookmarked (in any folder), hollow
    // otherwise — matching Chrome's star-reflects-state. The glyph still dims when
    // there is no live page.
    let (glyph, color) = match (has_page, is_bookmarked) {
        (false, _) => ("\u{2606}", Style::TEXT_DIM),
        (true, true) => ("\u{2605}", Style::ACCENT),
        (true, false) => ("\u{2606}", Style::TEXT),
    };
    let tip = if is_bookmarked {
        "Bookmarked \u{2014} page actions: edit bookmark, copy URL, share"
    } else {
        "Page actions \u{2014} bookmark, copy URL, share"
    };
    ui.menu_button(RichText::new(glyph).size(CHROME_FONT).color(color), |ui| {
        page_actions_menu(ui, bus_root, engine, url, title);
    })
    .response
    .on_hover_text(tip);
}

/// Whether the compact toolbar's reload slot should present as a real Stop
/// control instead of Reload.
///
/// Only CEF exposes a genuine cancel-load hook (`cef_browser_t::stop_load`,
/// offset-verified against the pinned CEF 149 headers — see
/// `mde-web-cef::cef_browser::apply_control_frame`). Servo's embedding API does
/// not: the official `servo`/`servo-embedder-traits`/`servo-constellation-traits`
/// 0.3.0 crates.io publications this workspace pins were inspected directly
/// (DD-2, 2026-07-10) — `WebView`'s and `Servo`'s complete public method sets
/// carry no stop/cancel-navigation method, the `WebDriverCommandMsg` relay
/// `Servo::execute_webdriver_command` accepts has no stop/cancel-navigation
/// variant (only `LoadUrl`/`Refresh`/`GoBack`/`GoForward` affect navigation),
/// and the `EmbedderToConstellationMessage` channel `WebView::load`/`reload`
/// send on (unreachable anyway — `WebView::inner()` is a private accessor)
/// has no such variant either. Returning `true` for a non-CEF engine would
/// paint a Stop button that silently does nothing when clicked, which is worse
/// than the honest Reload it would replace, so this stays a hard per-engine
/// check rather than a capability guess.
fn can_show_stop_control(
    has_tab: bool,
    crashed: bool,
    loading: bool,
    engine: Option<BrowserEngine>,
) -> bool {
    has_tab && !crashed && loading && engine == Some(BrowserEngine::Cef)
}

/// Chrome/Edge-style **trust signal** for the omnibox's leading security chip
/// (OMNIBOX-STYLE), derived purely from a URL's scheme. `Mesh` covers the
/// airgapped `mesh://` scheme — trusted the same way HTTPS is, since mesh
/// traffic never leaves the Nebula overlay and never touches the open web.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecurityLevel {
    /// `https://` — a lock glyph, neutral tone. Modern browsers stopped
    /// painting plain HTTPS green; it is simply the unremarkable default.
    Secure,
    /// `http://` — a "Not secure" glyph/tone; a deliberate downgrade signal,
    /// so the scheme itself also stays visible in the omnibox text (never
    /// elided the way `https://` is).
    NotSecure,
    /// `mesh://` and mesh-hosted services — a shield glyph, trusted.
    Mesh,
    /// `about:` / blank / new-tab / any other scheme — a neutral glyph.
    Neutral,
}

impl SecurityLevel {
    /// The chip's leading glyph.
    const fn glyph(self) -> &'static str {
        match self {
            Self::Secure => "\u{1F512}",   // lock
            Self::NotSecure => "\u{26A0}", // warning triangle
            Self::Mesh => "\u{1F6E1}",     // shield
            Self::Neutral => "\u{1F50E}",  // magnifying glass
        }
    }

    /// The chip's hover tooltip / accessible label.
    const fn label(self) -> &'static str {
        match self {
            Self::Secure => "Secure connection (HTTPS)",
            Self::NotSecure => "Not secure — plain HTTP",
            Self::Mesh => "Mesh — trusted overlay connection",
            Self::Neutral => "No connection security to report",
        }
    }

    /// The design-system tone the chip paints in (never a raw literal, §4).
    const fn tone(self) -> ChipTone {
        match self {
            Self::Secure | Self::Neutral => ChipTone::Neutral,
            Self::NotSecure => ChipTone::Warn,
            Self::Mesh => ChipTone::Info,
        }
    }
}

/// A short-list of common two-level public suffixes for the omnibox's eTLD+1
/// heuristic (OMNIBOX-STYLE). This is deliberately NOT a Public Suffix List (a
/// large vendored blob) — just enough of the common ccTLD-style suffixes that
/// the naive "last two dot-labels" rule would otherwise mis-emphasize
/// (`foo.co.uk` must emphasize `foo.co.uk`, not `co.uk`).
const OMNIBOX_TWO_LEVEL_SUFFIXES: &[&str] = &["co.uk", "com.au", "co.jp", "org.uk"];

/// The Chrome-style **display breakdown** of a URL, built by [`omnibox_display`]
/// for the unfocused omnibox (OMNIBOX-STYLE).
#[derive(Debug, Clone, PartialEq, Eq)]
struct OmniboxDisplay {
    /// The scheme prefix to render, if any. Always `None` for `https://` (the
    /// scheme is elided, Chrome-style); always `Some` for every other scheme,
    /// since keeping `http://` visible is a deliberate downgrade signal and
    /// every other scheme (`mesh://`, `about:`, …) needs to stay legible.
    scheme_shown: Option<String>,
    /// The host, with a leading `www.` already stripped. Empty when the URL
    /// carries no `scheme://host` authority (e.g. `about:blank`).
    host: String,
    /// Byte range into [`Self::host`] covering the registrable domain
    /// (eTLD+1, see [`OMNIBOX_TWO_LEVEL_SUFFIXES`]) — rendered in the
    /// strong/full-strength text token; the rest of `host` (a subdomain
    /// prefix) renders dimmed.
    host_emphasis: Range<usize>,
    /// Path + query + fragment after the host, rendered dimmed.
    rest: String,
    /// The scheme's trust signal, for the leading security chip.
    security: SecurityLevel,
}

/// Byte range of the eTLD+1 within `host` (dot-label heuristic, see
/// [`OMNIBOX_TWO_LEVEL_SUFFIXES`]). Falls back to the whole host for a bare
/// domain, `localhost`, or an IP literal (≤ 2 dot-labels) — a heuristic, not a
/// full Public Suffix List lookup.
fn omnibox_etld1_range(host: &str) -> Range<usize> {
    if host.is_empty() {
        return 0..0;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return 0..host.len();
    }
    let last_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    let take = if OMNIBOX_TWO_LEVEL_SUFFIXES.contains(&last_two.as_str()) {
        3
    } else {
        2
    }
    .min(labels.len());
    let start_label = labels.len() - take;
    let start: usize = labels[..start_label].iter().map(|l| l.len() + 1).sum();
    start..host.len()
}

/// Build the Chrome-style [`OmniboxDisplay`] breakdown for `url` — pure logic,
/// unit-tested directly (OMNIBOX-STYLE). Elides the `https://` scheme, strips a
/// leading `www.`, and emphasizes the registrable domain; `http://` and every
/// other scheme stay fully visible as an honest identity/downgrade signal.
fn omnibox_display(url: &str) -> OmniboxDisplay {
    let trimmed = url.trim();
    let (scheme_shown, security, after_scheme) =
        if let Some(rest) = trimmed.strip_prefix("https://") {
            (None, SecurityLevel::Secure, rest)
        } else if let Some(rest) = trimmed.strip_prefix("http://") {
            (Some("http://"), SecurityLevel::NotSecure, rest)
        } else if let Some(rest) = trimmed.strip_prefix("mesh://") {
            (Some("mesh://"), SecurityLevel::Mesh, rest)
        } else {
            // Any other scheme (`about:`, `data:`, `mailto:`, `ftp://`, …) — no
            // elision, no host emphasis: an honest, unmodified read-out.
            return OmniboxDisplay {
                scheme_shown: (!trimmed.is_empty()).then(|| trimmed.to_owned()),
                host: String::new(),
                host_emphasis: 0..0,
                rest: String::new(),
                security: SecurityLevel::Neutral,
            };
        };

    let split_at = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let (host_part, rest) = after_scheme.split_at(split_at);
    let host = host_part
        .strip_prefix("www.")
        .unwrap_or(host_part)
        .to_owned();
    let host_emphasis = omnibox_etld1_range(&host);

    OmniboxDisplay {
        scheme_shown: scheme_shown.map(str::to_owned),
        host,
        host_emphasis,
        rest: rest.to_owned(),
        security,
    }
}

/// Build the layout job painted over the omnibox when it is NOT focused
/// (OMNIBOX-STYLE): the scheme dimmed, the registrable domain in the strong
/// text token, and everything else (subdomain prefix + path/query) dimmed.
/// Returns an empty job for an empty `url` (the hint text shows through).
fn omnibox_layout_job(url: &str, font_id: egui::FontId) -> egui::text::LayoutJob {
    let display = omnibox_display(url);
    let mut job = egui::text::LayoutJob::default();
    let dim = egui::TextFormat {
        font_id: font_id.clone(),
        color: Style::TEXT_DIM,
        ..Default::default()
    };
    let strong = egui::TextFormat {
        font_id: font_id.clone(),
        color: Style::TEXT_STRONG,
        ..Default::default()
    };
    if display.host.is_empty() {
        // No parsed `scheme://host` authority (`about:`, `data:`, …) — one
        // neutral, unmodified run.
        if let Some(scheme) = &display.scheme_shown {
            job.append(scheme, 0.0, dim);
        }
        return job;
    }
    if let Some(scheme) = &display.scheme_shown {
        job.append(scheme, 0.0, dim.clone());
    }
    let Range { start, end } = display.host_emphasis;
    if start > 0 {
        job.append(&display.host[..start], 0.0, dim.clone());
    }
    job.append(&display.host[start..end], 0.0, strong);
    if end < display.host.len() {
        job.append(&display.host[end..], 0.0, dim.clone());
    }
    if !display.rest.is_empty() {
        job.append(&display.rest, 0.0, dim);
    }
    job
}

/// SECURITY-INFO — the plain-language headline for a [`SecurityLevel`], shown
/// at the top of the [`site_info_panel`]. Pure and paint-free so the copy is
/// directly unit-testable without driving a frame.
const fn security_headline(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::Secure => "Connection is secure",
        SecurityLevel::NotSecure => "Your connection to this site is not secure",
        SecurityLevel::Mesh => "Mesh service \u{2014} trusted overlay",
        SecurityLevel::Neutral => "About this page",
    }
}

/// SECURITY-INFO — the [`site_info_panel`]'s content, derived from the current
/// page URL. Built on top of [`omnibox_display`] rather than re-parsing the
/// URL, so the panel's host/eTLD+1 emphasis always matches the omnibox's own
/// read-out. Pure and paint-free, unit-tested directly.
struct SiteInfoSummary {
    security: SecurityLevel,
    headline: &'static str,
    /// The host, already `www.`-stripped — empty when the URL carries no
    /// `scheme://host` authority (e.g. `about:blank`).
    host: String,
    /// Byte range of the registrable domain within [`Self::host`] — see
    /// [`OmniboxDisplay::host_emphasis`].
    host_emphasis: Range<usize>,
    /// Set only for `https://` pages: this browser blocks cert-error
    /// navigations upstream (the TLS interstitial), so a live `https` page
    /// reaching this panel has already been certificate-validated.
    cert_line: Option<&'static str>,
    /// IDN homograph/punycode spoofing warning for the host, or `None` when the
    /// host is not a confusable risk — see [`confusable_warning`].
    confusable: Option<String>,
}

/// Human-readable IDN homograph/spoofing warning for `host`, or `None` when it is
/// not a confusable/punycode risk. Wires mde-adblock's [`confusable_reason`]
/// detector into the site-info panel — the place users click to confirm identity,
/// where a look-alike-domain warning belongs (the detector previously had no UI).
fn confusable_warning(host: &str) -> Option<String> {
    confusable_reason(host).map(|reason| {
        match reason {
            ConfusableReason::Punycode => {
                "Punycode/IDN host (xn--) \u{2014} verify this is the site you expect"
            }
            ConfusableReason::ConfusableBlock => {
                "Look-alike letters (Cyrillic/Greek) \u{2014} this host may impersonate another site"
            }
            ConfusableReason::MixedScript => {
                "Mixed-script host \u{2014} letters from more than one alphabet can spoof a name"
            }
        }
        .to_owned()
    })
}

fn site_info_summary(page_url: &str) -> SiteInfoSummary {
    let display = omnibox_display(page_url);
    let cert_line = matches!(display.security, SecurityLevel::Secure)
        .then_some("Certificate: valid \u{2014} the connection is encrypted");
    let confusable = confusable_warning(&display.host);
    SiteInfoSummary {
        security: display.security,
        headline: security_headline(display.security),
        host: display.host,
        host_emphasis: display.host_emphasis,
        cert_line,
        confusable,
    }
}

/// A stable, ui-path-independent id for the [`site_info_panel`] popup — a
/// single well-known key rather than [`egui::Ui::make_persistent_id`], so
/// tests can open/close it without replaying the chrome bar's exact widget
/// layout.
fn security_chip_popup_id() -> egui::Id {
    egui::Id::new("mde_web_security_chip_popup")
}

/// SECURITY-INFO — the Chrome-style "site information" popup opened by
/// clicking the omnibox's [`security_chip`]: the security headline, the
/// page's host (registrable domain emphasized, matching the omnibox), an
/// `https` cert-validity line, and the browser's session-only privacy note
/// (B3/Q74 — cookies and site data are never persisted past the browser
/// closing, so this stays a static fact rather than a live count).
fn site_info_panel(ui: &mut egui::Ui, page_url: &str) {
    let summary = site_info_summary(page_url);
    ui.set_max_width(260.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(summary.security.glyph())
                .size(CHROME_FONT)
                .color(summary.security.tone().color()),
        );
        ui.label(
            RichText::new(summary.headline)
                .color(summary.security.tone().color())
                .strong(),
        );
    });
    if summary.host.is_empty() {
        ui.label(RichText::new("No page is currently loaded").color(Style::TEXT_DIM));
    } else {
        let font_id = egui::FontId::new(CHROME_FONT, egui::FontFamily::Proportional);
        let mut job = egui::text::LayoutJob::default();
        let dim = egui::TextFormat {
            font_id: font_id.clone(),
            color: Style::TEXT_DIM,
            ..Default::default()
        };
        let strong = egui::TextFormat {
            font_id,
            color: Style::TEXT_STRONG,
            ..Default::default()
        };
        let Range { start, end } = summary.host_emphasis;
        if start > 0 {
            job.append(&summary.host[..start], 0.0, dim.clone());
        }
        job.append(&summary.host[start..end], 0.0, strong);
        if end < summary.host.len() {
            job.append(&summary.host[end..], 0.0, dim);
        }
        ui.label(job);
    }
    if let Some(warn) = summary.confusable.as_deref() {
        ui.label(
            RichText::new(format!("\u{26A0} {warn}"))
                .small()
                .color(Style::WARN),
        );
    }
    if let Some(cert_line) = summary.cert_line {
        ui.label(RichText::new(cert_line).small().color(Style::TEXT_DIM));
    }
    ui.separator();
    ui.label(
        RichText::new("Cookies & site data clear when you close the browser")
            .small()
            .color(Style::TEXT_DIM),
    );
}

/// OMNIBOX-STYLE — the leading security chip, reflecting the CURRENT
/// (committed) page URL's scheme, not the in-progress edit draft. Clicking it
/// opens the SECURITY-INFO [`site_info_panel`] below it, dismissed on
/// click-away or Esc (egui's built-in popup close behavior).
fn security_chip(ui: &mut egui::Ui, page_url: &str) {
    let security = omnibox_display(page_url).security;
    let popup_id = security_chip_popup_id();
    let resp = ui
        .add(
            egui::Button::new(
                RichText::new(security.glyph())
                    .size(CHROME_FONT)
                    .color(security.tone().color()),
            )
            .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
        )
        .on_hover_text(security.label());
    if resp.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    egui::popup_below_widget(
        ui,
        popup_id,
        &resp,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| site_info_panel(ui, page_url),
    );
}

/// The navigation chrome bar — a §4-token toolbar. Back / forward / reload act on
/// the active session; the address bar loads on submit. On a crashed tab, Reload
/// becomes a respawn request. The page-actions menu (BOOKMARKS-10) hangs off both
/// the toolbar star button and the address bar's right-click.
fn nav_chrome(ui: &mut egui::Ui, state: &mut WebState) {
    let crashed = state
        .tabs
        .get(state.active)
        .is_some_and(|t| t.session.is_crashed());
    let nav = state
        .tabs
        .get(state.active)
        .map(|t| t.session.nav().clone())
        .unwrap_or_default();
    let active_engine = state.tabs.get(state.active).map(|t| t.engine);
    let has_tab = !state.tabs.is_empty();
    // BOOKMARKS-7 — the per-page ad-filter blocked count the active session tracks.
    let blocked = state
        .tabs
        .get(state.active)
        .map_or(0, |t| t.session.blocked_count());
    // BOOKMARKS-10 — the live page's URL + title, owned clones so the page-actions
    // closures never borrow `state`; empty when there is no live tab to act on.
    let page_url = state
        .tabs
        .get(state.active)
        .map(|t| t.session.nav().url.clone())
        .unwrap_or_default();
    let page_title = state
        .tabs
        .get(state.active)
        .map(|t| t.session.title().to_string())
        .unwrap_or_default();
    let has_page = has_tab && !crashed && !page_url.trim().is_empty();

    let mut accepted_suggestion: Option<String> = None;
    ui.horizontal(|ui| {
        // Back / forward — enabled only when the live session offers the history.
        if nav_button(ui, "\u{2039}", "Back", has_tab && !crashed && nav.can_back) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_back();
            }
        }
        if nav_button(
            ui,
            "\u{203A}",
            "Forward",
            has_tab && !crashed && nav.can_forward,
        ) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_forward();
            }
        }
        // Stop while CEF is loading; otherwise Reload respawns crashed tabs or
        // reloads the page. Servo currently has no real cancel-load hook (DD-2,
        // investigated 2026-07-10 — see `can_show_stop_control`), so its compact
        // chrome keeps the honest Reload control while loading.
        let can_stop = can_show_stop_control(has_tab, crashed, nav.loading, active_engine);
        let (nav_label, nav_tip) = if can_stop {
            ("\u{00D7}", "Stop loading")
        } else if crashed {
            ("\u{21BB}", "Reload (restart page)")
        } else {
            ("\u{21BB}", "Reload")
        };
        if nav_button(ui, nav_label, nav_tip, has_tab) {
            if crashed {
                state.respawn_requested = true;
            } else if can_stop {
                if let Some(tab) = state.active_tab() {
                    tab.session.stop();
                }
            } else if let Some(tab) = state.active_tab() {
                tab.session.reload();
            }
        }

        // BOOKMARKS-10 — the page-actions menu (bookmark this page / copy its URL /
        // send it in Chat). The SAME three verbs also hang off the address bar's
        // right-click (below), so both the toolbar and the context menu reach them.
        let is_bookmarked = has_page
            && state
                .bookmarked_urls
                .contains(bookmark_membership_key(&page_url));
        page_actions_button(
            ui,
            has_page,
            is_bookmarked,
            state.bus_root.as_deref(),
            active_engine,
            &page_url,
            &page_title,
        );
        password_menu(ui, state, &page_url, has_page);

        if nav_button(
            ui,
            "\u{25A3}",
            if state.capture_region_mode {
                "Select capture region"
            } else {
                "Capture viewport"
            },
            state.active_tab_has_frame(),
        ) {
            if state.capture_region_mode {
                state.cancel_region_capture();
            } else {
                state.capture_active_viewport();
            }
        }

        let (active_downloads, total_downloads) = state.download_counts();
        let downloads_label = if active_downloads > 0 {
            format!("\u{2193} {active_downloads}")
        } else {
            "\u{2193}".to_owned()
        };
        let downloads_tip = if total_downloads == 0 {
            "Downloads"
        } else {
            "Downloads from the shared Transfers ledger"
        };
        if ui
            .button(RichText::new(downloads_label).size(CHROME_FONT).color(
                if state.downloads_open {
                    Style::ACCENT
                } else {
                    Style::TEXT
                },
            ))
            .on_hover_text(downloads_tip)
            .clicked()
        {
            state.downloads_open = !state.downloads_open;
            if state.downloads_open {
                state.refresh_downloads();
            }
        }

        // BOOKMARKS-7 — a compact "N blocked" shield when the ad-filter has dropped
        // requests on this page (honest 0 stays hidden). Reads the session's
        // per-page counter; the engine is compiled from the mackesd `adfilter` blob.
        if blocked > 0 {
            ui.add_space(CHROME_GAP);
            // The by-domain breakdown behind the count (uBlock-style detail): the top
            // blocked domains for THIS page, surfaced on hover of the shield.
            let top_blocked = state
                .tabs
                .get(state.active)
                .map(|t| t.session.block_tally().top_domains(6))
                .unwrap_or_default();
            ui.label(
                RichText::new(format!("\u{2298} {blocked}"))
                    .size(CHROME_FONT)
                    .color(Style::TEXT_DIM),
            )
            .on_hover_ui(|ui| {
                ui.label(format!(
                    "Ad-filter blocked {blocked} request{} on this page",
                    if blocked == 1 { "" } else { "s" }
                ));
                if !top_blocked.is_empty() {
                    ui.separator();
                    for (domain, count) in &top_blocked {
                        ui.label(
                            RichText::new(format!("{count}\u{00D7}  {domain}"))
                                .small()
                                .color(Style::TEXT_DIM),
                        );
                    }
                }
            });
        }

        ui.add_space(CHROME_GAP);

        // OMNIBOX-STYLE — the leading security chip reflects the CURRENT
        // (committed) page URL's scheme, never the in-progress edit draft.
        security_chip(ui, &page_url);
        ui.add_space(CHROME_GAP);

        // The address bar fills the rest of the row.
        let field = egui::TextEdit::singleline(&mut state.address)
            .id(omnibox_widget_id())
            .desired_width(ui.available_width() - Style::SP_XL * 2.0)
            .hint_text("Enter an address")
            .text_color(Style::TEXT)
            .font(egui::TextStyle::Small)
            .min_size(egui::vec2(160.0, CHROME_OMNIBOX_H));
        let resp = ui.add_enabled(has_tab && !crashed, field);
        // Latch omnibox focus for next frame's engine-sync + accelerator
        // guards (the same tracked-focus idiom as `Tab::page_focused`).
        state.omnibox_focused = resp.has_focus();
        state.chrome_edit_focus |= resp.has_focus();
        // The branded 2px accent focus ring (mde_egui::focus, the dock/Console/Start
        // idiom) on the primary keyboard target, so keyboard-only users get a clear,
        // consistent focus indicator instead of egui's faint default outline (a11y).
        mde_egui::focus::paint_focus_ring(ui.painter(), resp.rect, resp.has_focus());
        // OMNIBOX-STYLE — when the omnibox is NOT being edited, paint the
        // Chrome-style elided/emphasized read-out ON TOP of the TextEdit
        // (never touching its own layouter/cursor logic, so click-to-edit
        // cursor placement stays exactly as correct as it is today). Focused
        // editing always shows the full, unmodified draft underneath.
        if has_tab && !crashed && !resp.has_focus() && !state.address.trim().is_empty() {
            let font_id = egui::FontId::new(CHROME_FONT, egui::FontFamily::Proportional);
            let job = omnibox_layout_job(&state.address, font_id);
            if !job.is_empty() {
                let galley = ui.fonts(|f| f.layout_job(job));
                let bg = ui.visuals().extreme_bg_color;
                let corner_radius = ui.visuals().widgets.inactive.corner_radius;
                ui.painter().rect_filled(resp.rect, corner_radius, bg);
                let text_pos = egui::pos2(
                    resp.rect.left() + 4.0,
                    resp.rect.center().y - galley.size().y / 2.0,
                );
                ui.painter().galley(text_pos, galley, Style::TEXT);
            }
        }
        if resp.changed() && has_tab && !crashed {
            state.update_suggestions_for_address();
        }
        // Keyboard-navigate the suggestion dropdown: a single-line TextEdit ignores
        // vertical arrows, so intercepting Up/Down here (while the omnibox has focus)
        // is free and doesn't disturb caret motion. Enter then commits the highlight.
        if resp.has_focus() && has_tab && !crashed {
            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                state.suggestions.move_selection(1);
            } else if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                state.suggestions.move_selection(-1);
            }
        }
        // BOOKMARKS-10 — right-click the address bar for the same page actions
        // (bookmark / copy URL / Send-in-Chat) the toolbar star exposes.
        resp.context_menu(|ui| {
            page_actions_menu(
                ui,
                state.bus_root.as_deref(),
                active_engine,
                &page_url,
                &page_title,
            );
        });
        let submit = resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && has_tab
            && !crashed;

        let go = ui
            .add_enabled(
                has_tab && !crashed && !state.address.trim().is_empty(),
                egui::Button::new(
                    RichText::new("\u{2192}")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                )
                .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
            )
            .on_hover_text("Go")
            .clicked();

        if submit {
            // Enter commits the keyboard-highlighted suggestion if one is selected,
            // else the typed draft — Chrome's omnibox behavior.
            if let Some(selected) = state.suggestions.selected_value() {
                state.accept_suggestion(selected);
            } else {
                state.submit_address();
            }
        } else if go {
            state.submit_address();
        }
    });
    if has_tab && !crashed {
        accepted_suggestion = suggestions_panel(ui, state);
    }
    if let Some(suggestion) = accepted_suggestion {
        state.accept_suggestion(suggestion);
    }
}

/// Stable egui id for the omnibox TextEdit, so its keyboard focus can be
/// tracked across frames (and driven by the tests).
fn omnibox_widget_id() -> egui::Id {
    egui::Id::new("browser-omnibox")
}

/// BOOKMARKS-BAR — the single-row horizontal bookmarks bar below the nav chrome.
///
/// Renders the top-level bookmarks mirrored from `state/bookmarks/collection` as
/// elided-title buttons: a plain click navigates the active tab, a middle-click
/// opens a new tab. When the row can't hold them all, the tail collapses into a
/// ">>" overflow menu ([`bookmark_bar_visible_count`] does the exact fixed-width
/// split). Hidden unless the View → Show Bookmarks Bar toggle is on; when shown
/// but empty it carries an honest hint rather than a blank strip (§7). Look reads
/// only from `Style` (no raw colour), matching the sibling chrome rows.
fn bookmarks_bar(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.bookmarks_bar_visible {
        return;
    }
    // Clone the small link row so the click closures can drive `&mut state` after
    // the layout closure (the find-bar's collect-then-act pattern).
    let links = state.bookmark_bar_links.clone();
    // (url, open_in_new_tab) chosen this frame, applied once after the layout.
    let mut chosen: Option<(String, bool)> = None;
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            if links.is_empty() {
                muted_note(
                    ui,
                    "No bookmarks yet \u{2014} add one from Bookmarks \u{2192} Add Bookmark",
                );
                return;
            }
            ui.horizontal(|ui| {
                let avail = ui.available_width();
                let visible = bookmark_bar_visible_count(
                    links.len(),
                    avail,
                    BOOKMARK_BTN_W,
                    CHROME_GAP,
                    BOOKMARK_OVERFLOW_W,
                );
                for link in &links[..visible] {
                    let resp = ui
                        .add(
                            egui::Button::new(
                                RichText::new(ellipsize(&link.title, BOOKMARK_TITLE_CHARS))
                                    .size(CHROME_FONT)
                                    .color(Style::TEXT),
                            )
                            .fill(Style::SURFACE)
                            .min_size(egui::vec2(BOOKMARK_BTN_W, CHROME_BUTTON)),
                        )
                        .on_hover_text(format!("{}\n{}", link.title, link.url));
                    if resp.clicked() {
                        chosen = Some((link.url.clone(), false));
                    } else if resp.middle_clicked() {
                        chosen = Some((link.url.clone(), true));
                    }
                }
                if visible < links.len() {
                    ui.menu_button(
                        RichText::new("\u{00BB}")
                            .size(CHROME_FONT)
                            .color(Style::TEXT),
                        |ui| {
                            for link in &links[visible..] {
                                let resp = ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(ellipsize(&link.title, 40))
                                                .size(CHROME_FONT)
                                                .color(Style::TEXT),
                                        )
                                        .fill(Style::SURFACE),
                                    )
                                    .on_hover_text(link.url.clone());
                                if resp.clicked() {
                                    chosen = Some((link.url.clone(), false));
                                    ui.close_menu();
                                } else if resp.middle_clicked() {
                                    chosen = Some((link.url.clone(), true));
                                    ui.close_menu();
                                }
                            }
                        },
                    )
                    .response
                    .on_hover_text("More bookmarks");
                }
            });
        });
    if let Some((url, new_tab)) = chosen {
        state.open_bookmark(url, new_tab);
    }
}

fn find_chrome(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.find_open {
        return;
    }
    let enabled = state.can_drive_page_tools();
    // The engine's match tally, shown as "active/count" (or "No results"). Read
    // before the mutable-borrow closure below.
    let find_tally = (!state.find_query.trim().is_empty())
        .then(|| state.active_find_result())
        .flatten();
    let mut submit_forward = false;
    let mut submit_backward = false;
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Find")
                        .size(CHROME_FONT)
                        .color(Style::TEXT_DIM),
                );
                let resp = ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut state.find_query)
                        .desired_width(220.0)
                        .hint_text("Find in page")
                        .text_color(Style::TEXT)
                        .font(egui::TextStyle::Small)
                        .min_size(egui::vec2(160.0, CHROME_OMNIBOX_H)),
                );
                state.chrome_edit_focus |= resp.has_focus();
                if let Some((active, count)) = find_tally {
                    let label = if count == 0 {
                        "No results".to_owned()
                    } else {
                        format!("{active}/{count}")
                    };
                    ui.label(
                        RichText::new(label)
                            .size(CHROME_FONT)
                            .color(Style::TEXT_DIM),
                    );
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter && ui.input(|i| i.modifiers.shift) {
                    submit_backward = true;
                } else if enter {
                    submit_forward = true;
                }
                if nav_button(ui, "\u{2191}", "Previous match", enabled) {
                    submit_backward = true;
                }
                if nav_button(ui, "\u{2193}", "Next match", enabled) {
                    submit_forward = true;
                }
                if nav_button(ui, "\u{00D7}", "Close find", true) {
                    state.close_find_bar();
                }
            });
        });
    if submit_backward {
        state.submit_find(true);
    } else if submit_forward {
        state.submit_find(false);
    }
}

fn short_transfer_name(job: &TransferJob) -> String {
    job.source
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .map_or_else(|| job.id.clone(), ToOwned::to_owned)
}

const fn download_state_color(state: TransferState) -> egui::Color32 {
    match state {
        TransferState::Done => Style::OK,
        TransferState::Failed => Style::DANGER,
        TransferState::Paused => Style::WARN,
        TransferState::Queued | TransferState::Running => Style::TEXT_DIM,
    }
}

/// Omnibox search `items` with any entry that duplicates a history hit removed
/// (a history-matched URL is already shown once, above, by
/// [`suggestions_panel`] — Chrome-style history-then-search ordering with no
/// repeats). Pure and paint-free so it's directly unit-testable.
fn dedup_search_items<'a>(items: &'a [String], history: &[String]) -> Vec<&'a String> {
    items.iter().filter(|s| !history.contains(s)).collect()
}

fn suggestions_panel(ui: &mut egui::Ui, state: &WebState) -> Option<String> {
    let history = &state.suggestions.history;
    let bookmarks = &state.suggestions.bookmarks;
    let search_items = dedup_search_items(&state.suggestions.items, history);
    if bookmarks.is_empty()
        && history.is_empty()
        && search_items.is_empty()
        && state.suggestions.notice.is_none()
    {
        return None;
    }
    let mut accepted = None;
    // Flat render index tracking the keyboard highlight ([`SuggestionState::selected`])
    // across the bookmark → history → search sections, so a highlighted row gets an
    // accent fill and Up/Down move visibly.
    let selected = state.suggestions.selected;
    let mut idx = 0usize;
    let fill_for = |idx: usize| {
        if Some(idx) == selected {
            Style::SURFACE_HI
        } else {
            Style::SURFACE
        }
    };
    ui.horizontal_wrapped(|ui| {
        ui.add_space(Style::SP_XL * 4.0);
        if !bookmarks.is_empty() {
            muted_note(ui, "Bookmarks");
            for bm in bookmarks {
                let clicked = ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("\u{2605} {}", ellipsize(&bm.title, 32)))
                                .size(CHROME_FONT)
                                .color(Style::ACCENT),
                        )
                        .fill(fill_for(idx))
                        .min_size(egui::vec2(96.0, CHROME_BUTTON)),
                    )
                    .on_hover_text(format!("Bookmark: {}", bm.url))
                    .clicked();
                if clicked {
                    accepted = Some(bm.url.clone());
                }
                idx += 1;
            }
        }
        if !history.is_empty() {
            muted_note(ui, "History");
            for url in history {
                let clicked = ui
                    .add(
                        egui::Button::new(
                            RichText::new(ellipsize(url, 36))
                                .size(CHROME_FONT)
                                .color(Style::TEXT),
                        )
                        .fill(fill_for(idx))
                        .min_size(egui::vec2(96.0, CHROME_BUTTON)),
                    )
                    .on_hover_text(format!("Visited: {url}"))
                    .clicked();
                if clicked {
                    accepted = Some(url.clone());
                }
                idx += 1;
            }
        }
        for suggestion in search_items {
            let clicked = ui
                .add(
                    egui::Button::new(
                        RichText::new(ellipsize(suggestion, 36))
                            .size(CHROME_FONT)
                            .color(Style::TEXT),
                    )
                    .fill(fill_for(idx))
                    .min_size(egui::vec2(96.0, CHROME_BUTTON)),
                )
                .on_hover_text(format!("Search for {suggestion}"))
                .clicked();
            if clicked {
                accepted = Some(suggestion.clone());
            }
            idx += 1;
        }
        if state.suggestions.items.is_empty() && history.is_empty() {
            if let Some(notice) = state.suggestions.notice.as_deref() {
                muted_note(ui, notice);
            }
        }
    });
    accepted
}

fn insecure_prompt(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(url) = state.insecure_prompt.clone() else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new("HTTP connection")
                .size(CHROME_FONT)
                .color(Style::WARN),
        );
        ui.label(RichText::new(ellipsize(&url, 64)).color(Style::TEXT_DIM));
        if ui
            .add(egui::Button::new(
                RichText::new("Use HTTPS")
                    .size(CHROME_FONT)
                    .color(Style::TEXT),
            ))
            .on_hover_text("Upgrade this navigation to HTTPS")
            .clicked()
        {
            state.upgrade_insecure_load();
        }
        if ui
            .add(egui::Button::new(
                RichText::new("Continue HTTP")
                    .size(CHROME_FONT)
                    .color(Style::WARN),
            ))
            .on_hover_text("Continue with the insecure HTTP URL")
            .clicked()
        {
            state.continue_insecure_load();
        }
        if ui
            .add(egui::Button::new(
                RichText::new("Cancel")
                    .size(CHROME_FONT)
                    .color(Style::TEXT_DIM),
            ))
            .clicked()
        {
            state.cancel_insecure_load();
        }
    });
}

fn new_tab_dashboard(ui: &mut egui::Ui, state: &mut WebState) {
    let mut submit_search = false;
    let mut open_service: Option<String> = None;
    centered(ui, |ui| {
        ui.label(
            RichText::new("Quasar Browser")
                .size(Style::HEADING)
                .color(Style::TEXT),
        );
        // Private-by-default explainer — the browser has no persistent profile
        // (sandbox has no writable $HOME); make that posture legible on the front
        // door instead of only in the Privacy menu (industry-grade "private mode UX").
        ui.label(
            RichText::new(PRIVATE_MODE_EXPLAINER)
                .small()
                .color(Style::TEXT_DIM),
        );
        ui.add_space(Style::SP_M);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.dashboard_query)
                    .desired_width(420.0)
                    .hint_text("Search the mesh")
                    .text_color(Style::TEXT),
            );
            state.chrome_edit_focus |= resp.has_focus();
            submit_search = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add(egui::Button::new(
                    RichText::new("Search").color(Style::TEXT),
                ))
                .clicked()
            {
                submit_search = true;
            }
        });
        ui.add_space(Style::SP_M);
        ui.horizontal_wrapped(|ui| {
            for service in &state.speed_dial {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(service.label.as_str())
                                .size(Style::BODY)
                                .color(Style::TEXT),
                        )
                        .min_size(egui::vec2(112.0, Style::SP_XL)),
                    )
                    .on_hover_text(service.hint.as_str())
                    .clicked()
                {
                    open_service = Some(service.url.clone());
                }
            }
        });
    });
    if submit_search {
        state.submit_dashboard_search();
    }
    if let Some(url) = open_service {
        state.open_mesh_service(url);
    }
}

/// A compact chrome button in the §4 palette, returning whether it was clicked.
fn nav_button(ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool) -> bool {
    let color = if enabled {
        Style::TEXT
    } else {
        Style::TEXT_DIM
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(glyph).size(CHROME_FONT).color(color))
            .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
    )
    .on_hover_text(tip)
    .clicked()
}

/// Map a pointer position from egui panel space into the helper frame's **device
/// pixels** — the ONE transform both the live-input forward and the region-capture
/// drag flow through (browser-1).
///
/// The decoded frame is painted to *fill* `image_rect`, which sits below the tab
/// strip + nav chrome (so its origin is non-zero) and whose size — reported in egui
/// points — differs from the frame's device-pixel size on any non-1:1 seat (`HiDPI`,
/// maximized, 4K, a non-frame aspect). A pointer at fraction `f` across `image_rect`
/// maps to the same fraction across the `frame_size` device grid, so the transform
///
/// 1. clamps the pointer into `image_rect`,
/// 2. subtracts the rect origin and divides by the rect size for a `0..1` fraction
///    (both pointer and rect are egui points, so `pixels_per_point` cancels — the
///    mapping is DPI-independent), then
/// 3. multiplies by `frame_size` to land in frame device pixels, bounded to
///    `[0, frame_w] × [0, frame_h]`.
///
/// The old live path instead multiplied `pos - image_rect.min` by `pixels_per_point`
/// against a *fixed* 1280×800 frame, so clicks landed at the wrong page coordinate
/// on every seat whose displayed rect wasn't exactly 1280×800 device px.
fn map_pointer_to_frame(
    pos: egui::Pos2,
    image_rect: egui::Rect,
    frame_size: [usize; 2],
) -> egui::Pos2 {
    let clamped = pos.clamp(image_rect.min, image_rect.max);
    let rel = clamped - image_rect.min;
    egui::pos2(
        rel.x * frame_size[0] as f32 / image_rect.width().max(1.0),
        rel.y * frame_size[1] as f32 / image_rect.height().max(1.0),
    )
}

/// The device-pixel size the helper's frame should track — the browser panel `rect`
/// (egui points) scaled by `pixels_per_point`, clamped to [`MAX_CHANNEL_DIM`] so a
/// resize can never exceed the pre-sized channel. Rounded to whole pixels and at
/// least 1×1 (browser-1, item 2).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "device extent is scaled, rounded, then clamped into [1, MAX_CHANNEL_DIM]"
)]
fn frame_target_device_px(rect: egui::Rect, ppp: f32) -> (u32, u32) {
    let dim = |v: f32| -> u32 {
        if v.is_finite() {
            (v * ppp).round().clamp(1.0, MAX_CHANNEL_DIM as f32) as u32
        } else {
            1
        }
    };
    (dim(rect.width()), dim(rect.height()))
}

/// Paint the active tab's decoded frame to fill the body and forward this frame's
/// egui input to the session. Pointer geometry is mapped into frame device pixels
/// via [`map_pointer_to_frame`], and the helper's viewport is re-sized (debounced)
/// to track the real panel (browser-1).
/// Map the engine-neutral [`CursorKind`] onto the shell's [`egui::CursorIcon`].
fn cursor_icon_for(kind: mde_web_preview_client::CursorKind) -> egui::CursorIcon {
    use mde_web_preview_client::CursorKind as K;
    match kind {
        K::Default => egui::CursorIcon::Default,
        K::Pointer => egui::CursorIcon::PointingHand,
        K::Text => egui::CursorIcon::Text,
        K::Crosshair => egui::CursorIcon::Crosshair,
        K::Wait => egui::CursorIcon::Wait,
        K::Progress => egui::CursorIcon::Progress,
        K::Help => egui::CursorIcon::Help,
        K::Move => egui::CursorIcon::Move,
        K::Grab => egui::CursorIcon::Grab,
        K::Grabbing => egui::CursorIcon::Grabbing,
        K::NotAllowed => egui::CursorIcon::NotAllowed,
        K::ResizeHorizontal => egui::CursorIcon::ResizeHorizontal,
        K::ResizeVertical => egui::CursorIcon::ResizeVertical,
        K::ResizeNeSw => egui::CursorIcon::ResizeNeSw,
        K::ResizeNwSe => egui::CursorIcon::ResizeNwSe,
        K::ZoomIn => egui::CursorIcon::ZoomIn,
        K::ZoomOut => egui::CursorIcon::ZoomOut,
    }
}

/// An action chosen from the in-page right-click context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageContextAction {
    Back,
    Forward,
    Reload,
    Edit(EditCommand),
}

fn paint_body(ui: &mut egui::Ui, state: &mut WebState, active: usize) {
    let Some((tex_id, texture_size, frame_size)) = state.tabs.get(active).and_then(|tab| {
        let texture = tab.texture.as_ref()?;
        Some((
            texture.id(),
            texture.size_vec2(),
            tab.last_frame.as_ref().map_or([0, 0], |frame| frame.size),
        ))
    }) else {
        return;
    };
    let size = ui.available_size();
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let image_rect = fit_rect_preserving_aspect(rect, texture_size);
    ui.painter().rect_filled(rect, 0.0, Style::SURFACE);
    egui::Image::new(egui::load::SizedTexture::new(tex_id, image_rect.size()))
        .paint_at(ui, image_rect);

    // Reflect the engine's cursor (hand over links, I-beam over text, resize
    // edges, …) while the pointer is over the page canvas.
    if !state.capture_region_mode && resp.hovered() {
        if let Some(kind) = state.tabs.get(active).map(|tab| tab.session.cursor()) {
            ui.output_mut(|o| o.cursor_icon = cursor_icon_for(kind));
        }
    }

    // In-page right-click context menu: navigation + native clipboard/edit
    // commands on the focused element (driven through the engine, not JS).
    if !state.capture_region_mode {
        let (can_back, can_forward, url) = state.tabs.get(active).map_or_else(
            || (false, false, String::new()),
            |tab| {
                let nav = tab.session.nav();
                (nav.can_back, nav.can_forward, nav.url.clone())
            },
        );
        let mut action: Option<PageContextAction> = None;
        resp.context_menu(|ui| {
            if ui
                .add_enabled(can_back, egui::Button::new("Back"))
                .clicked()
            {
                action = Some(PageContextAction::Back);
                ui.close_menu();
            }
            if ui
                .add_enabled(can_forward, egui::Button::new("Forward"))
                .clicked()
            {
                action = Some(PageContextAction::Forward);
                ui.close_menu();
            }
            if ui.button("Reload").clicked() {
                action = Some(PageContextAction::Reload);
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Cut").clicked() {
                action = Some(PageContextAction::Edit(EditCommand::Cut));
                ui.close_menu();
            }
            if ui.button("Copy").clicked() {
                action = Some(PageContextAction::Edit(EditCommand::Copy));
                ui.close_menu();
            }
            if ui.button("Paste").clicked() {
                action = Some(PageContextAction::Edit(EditCommand::Paste));
                ui.close_menu();
            }
            if ui.button("Select all").clicked() {
                action = Some(PageContextAction::Edit(EditCommand::SelectAll));
                ui.close_menu();
            }
            ui.separator();
            if ui
                .add_enabled(!url.is_empty(), egui::Button::new("Copy page URL"))
                .clicked()
            {
                ui.ctx().copy_text(url.clone());
                ui.close_menu();
            }
        });
        if let Some(action) = action {
            state.apply_page_context_action(active, action);
        }
    }

    // Drive the helper's CSS viewport to the real panel size (device px), debounced
    // so a drag-resize sends ONE settled resize instead of one per frame — this
    // makes the page track the panel instead of a fixed 1280×800 breakpoint
    // (browser-1, item 2). Runs every frame, in capture mode too, so the tracked
    // size never drifts. Repaint while settling so the debounce fires without input.
    let ppp = ui.ctx().pixels_per_point();
    let target = frame_target_device_px(rect, ppp);
    if let Some(tab) = state.tabs.get_mut(active) {
        if let Some((w, h)) = tab.resizer.observe(target, Instant::now(), RESIZE_DEBOUNCE) {
            tab.session.resize(w, h);
        }
        if tab.resizer.is_settling() {
            ui.ctx().request_repaint_after(RESIZE_DEBOUNCE);
        }
    }

    if state.capture_region_mode {
        handle_region_capture_drag(ui, state, &resp, image_rect, frame_size);
    }
    if resp.clicked() {
        resp.request_focus();
    }

    if state.capture_region_mode {
        return;
    }
    // Forward only page-owned input. Pointer geometry is mapped into frame device
    // pixels (via `map_pointer_to_frame` inside `browser_input_event`); keyboard/text
    // belongs to the page only after the image has focus, so address-bar/chrome
    // typing does not leak into the helper.
    let mut page_focused = state.tabs.get(active).is_some_and(|tab| tab.page_focused)
        || resp.has_focus()
        || resp.clicked()
        || resp.dragged();
    for event in ui.input(|i| i.events.clone()) {
        if let egui::Event::PointerButton { pos, pressed, .. } = &event {
            if *pressed {
                if image_rect.contains(*pos) {
                    page_focused = true;
                    resp.request_focus();
                } else if !rect.contains(*pos) {
                    page_focused = false;
                }
            }
        }
        // IME composition (CJK / dead-key input) routes to the focused page via the
        // browser-host IME methods, NOT `send_input` — so it never reaches chrome.
        if page_focused {
            if let egui::Event::Ime(ime) = &event {
                if let Some(tab) = state.tabs.get_mut(active) {
                    match ime {
                        egui::ImeEvent::Preedit(text) => {
                            tab.session.ime_set_composition(text.clone());
                        }
                        egui::ImeEvent::Commit(text) => {
                            tab.session.ime_commit_text(text.clone());
                        }
                        egui::ImeEvent::Disabled => tab.session.ime_finish_composition(),
                        egui::ImeEvent::Enabled => {}
                    }
                    tab.last_activity = Instant::now();
                    tab.idle_suspended = false;
                }
                continue;
            }
        }
        if let Some(event) = browser_input_event(&event, image_rect, frame_size, page_focused) {
            if let Some(tab) = state.tabs.get_mut(active) {
                tab.last_activity = Instant::now();
                tab.idle_suspended = false;
                tab.session.send_input(&event, ppp);
            }
        }
    }
    if let Some(tab) = state.tabs.get_mut(active) {
        tab.page_focused = page_focused;
    }
    // While the page body owns input, ask the OS to route IME to it (anchored to the
    // body — the shell has no page-caret feedback, so the candidate window sits at the
    // body's top-left). Only set when the body is focused, so a focused omnibox keeps
    // its own IME. This is what makes the OS emit the `Event::Ime` stream handled above.
    if page_focused {
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                rect: image_rect,
                cursor_rect: egui::Rect::from_min_size(
                    image_rect.left_top(),
                    egui::vec2(1.0, 18.0),
                ),
            });
        });
    }
    if let Some(tab) = state.tabs.get(active) {
        install_browser_page_accessibility(ui.ctx(), image_rect, tab, page_focused);
    }
}

fn handle_region_capture_drag(
    ui: &mut egui::Ui,
    state: &mut WebState,
    resp: &egui::Response,
    image_rect: egui::Rect,
    frame_size: [usize; 2],
) {
    if frame_size[0] == 0 || frame_size[1] == 0 {
        state.cancel_region_capture();
        state.capture_notice = Some("Capture failed: no painted page".to_owned());
        return;
    }
    // The SAME transform the live-input path uses (browser-1 dedup).
    let pointer_to_frame = |pos: egui::Pos2| map_pointer_to_frame(pos, image_rect, frame_size);
    if resp.drag_started() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let pos = pointer_to_frame(pos);
            state.capture_region_start = Some(pos);
            state.capture_region_current = Some(pos);
        }
    } else if resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            state.capture_region_current = Some(pointer_to_frame(pos));
        }
    }

    if let (Some(start), Some(current)) = (state.capture_region_start, state.capture_region_current)
    {
        if let Some(region) = PixelRegion::from_points(start, current, frame_size) {
            let overlay = region.rect_on_image(image_rect, frame_size);
            ui.painter()
                .rect_filled(overlay, 0.0, Style::selection_wash());
            ui.painter().rect_stroke(
                overlay,
                0.0,
                egui::Stroke::new(1.0, Style::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
    }

    if resp.drag_stopped() {
        let result = state
            .capture_region_start
            .zip(state.capture_region_current)
            .and_then(|(start, current)| PixelRegion::from_points(start, current, frame_size))
            .ok_or_else(|| "selection is too small".to_owned())
            .and_then(|region| state.capture_active_region_to_dir(browser_capture_dir(), region));
        match result {
            Ok(path) => state.record_capture_success("Captured region", &path),
            Err(err) => state.capture_notice = Some(format!("Capture failed: {err}")),
        }
        state.cancel_region_capture();
    }
}

fn capture_notice(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(notice) = state.capture_notice.clone() else {
        return;
    };
    let tone = if notice.starts_with("Capture failed:")
        || notice.starts_with("PDF failed")
        || notice.starts_with("PDF viewer failed:")
        || notice.starts_with("Print failed:")
    {
        Style::DANGER
    } else {
        Style::ACCENT
    };
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(tone, RichText::new(notice).size(Style::SMALL));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Dismiss capture notice")
                        .clicked()
                    {
                        state.capture_notice = None;
                    }
                });
            });
        });
}

fn spellcheck_results_text(misses: &[SpellMiss]) -> String {
    misses
        .iter()
        .map(|miss| {
            let suggestions = if miss.suggestions.is_empty() {
                "no suggestions".to_owned()
            } else {
                miss.suggestions.join(", ")
            };
            format!(
                "{} [{}..{}]: {}",
                miss.word, miss.chars.start, miss.chars.end, suggestions
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn offline_cache_viewport_texture(
    ctx: &egui::Context,
    cache_id: &str,
    viewport: &OfflineCacheViewportImage,
) -> Option<TextureHandle> {
    let data_sig = offline_cache_viewport_data_sig(&viewport.data_base64);
    let key = egui::Id::new(("browser-offline-cache-viewport", cache_id, data_sig));
    if let Some(cached) = ctx.data_mut(|data| data.get_temp::<OfflineCacheViewportTexture>(key)) {
        if cached.data_sig == data_sig {
            return cached.texture;
        }
    }

    let texture = base64::engine::general_purpose::STANDARD
        .decode(viewport.data_base64.as_str())
        .ok()
        .and_then(|bytes| crate::chooser::decode_png_rgba(&bytes))
        .filter(|image| image.size == [viewport.width, viewport.height])
        .map(|image| {
            ctx.load_texture(
                format!("browser-offline-cache-viewport::{cache_id}"),
                image,
                TextureOptions::LINEAR,
            )
        });
    ctx.data_mut(|data| {
        data.insert_temp(
            key,
            OfflineCacheViewportTexture {
                data_sig,
                texture: texture.clone(),
            },
        );
    });
    texture
}

fn offline_cache_viewport_data_sig(data_base64: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    data_base64.hash(&mut hasher);
    hasher.finish()
}

fn offline_cache_viewport_display_size(
    ui: &egui::Ui,
    viewport: &OfflineCacheViewportImage,
) -> egui::Vec2 {
    let natural = egui::vec2(viewport.width as f32, viewport.height as f32);
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return egui::vec2(1.0, 1.0);
    }
    let max = egui::vec2(ui.available_width().max(1.0), 180.0);
    let scale = (max.x / natural.x).min(max.y / natural.y).min(1.0);
    natural * scale
}

fn fit_rect_preserving_aspect(outer: egui::Rect, content_size: egui::Vec2) -> egui::Rect {
    if content_size.x <= 0.0
        || content_size.y <= 0.0
        || outer.width() <= 0.0
        || outer.height() <= 0.0
    {
        return outer;
    }
    let scale = (outer.width() / content_size.x).min(outer.height() / content_size.y);
    let size = content_size * scale;
    egui::Rect::from_center_size(outer.center(), size)
}

fn accesskit_rect(rect: egui::Rect) -> egui::accesskit::Rect {
    egui::accesskit::Rect {
        x0: rect.min.x.into(),
        y0: rect.min.y.into(),
        x1: rect.max.x.into(),
        y1: rect.max.y.into(),
    }
}

fn browser_accessibility_id() -> egui::Id {
    egui::Id::new("browser-accessibility-status")
}

fn browser_page_accessibility_id() -> egui::Id {
    egui::Id::new("browser-accessibility-page")
}

fn tab_accessibility_state(tab: &Tab) -> String {
    if tab.idle_suspended {
        return "idle suspended".to_owned();
    }
    match tab.session.state() {
        SessionState::Loading => "loading".to_owned(),
        SessionState::Live => {
            if tab.texture.is_some() {
                "live".to_owned()
            } else {
                "live, waiting for first painted frame".to_owned()
            }
        }
        SessionState::Crashed { reason } => format!("crashed: {reason}"),
    }
}

fn tab_accessibility_tools(tab: &Tab) -> String {
    let mut tools = Vec::new();
    if tab.muted {
        tools.push("muted");
    }
    if tab.force_dark {
        tools.push("force dark");
    }
    if tab.reader_mode {
        tools.push("reader mode");
    }
    if tab.user_scripts {
        tools.push("userscripts");
    }
    if tab.page_focused {
        tools.push("page keyboard focus");
    }
    if tools.is_empty() {
        "no page tools enabled".to_owned()
    } else {
        tools.join(", ")
    }
}

fn tab_accessibility_summary(tab: &Tab) -> String {
    let nav = tab.session.nav();
    let title = tab.session.title().trim();
    let title = if title.is_empty() { "Untitled" } else { title };
    let url = nav.url.trim();
    let url = if url.is_empty() {
        "no committed URL"
    } else {
        url
    };
    let security = if url.starts_with("https://") {
        "secure"
    } else if url.starts_with("http://") {
        "not secure"
    } else {
        "local or internal"
    };
    format!(
        "{} page, {title}, {url}, {}, {}, container {}, display target {}, {}",
        tab.engine.label(),
        tab_accessibility_state(tab),
        security,
        tab.container.label(),
        tab.display_target.label(),
        tab_accessibility_tools(tab)
    )
}

fn browser_gate_notice(state: &WebState) -> &str {
    const DEFAULT_NOTICE: &str = "No live browser helper session is attached on this build or seat";
    #[cfg(feature = "live-helper")]
    {
        state.gate_notice.as_deref().unwrap_or(DEFAULT_NOTICE)
    }
    #[cfg(not(feature = "live-helper"))]
    {
        let _ = state;
        DEFAULT_NOTICE
    }
}

fn browser_accessibility_summary(state: &WebState) -> String {
    match state.tabs.get(state.active) {
        Some(tab) => format!(
            "Browser. Active tab {} of {}. {}",
            state.active + 1,
            state.tabs.len(),
            tab_accessibility_summary(tab)
        ),
        None => {
            let notice = browser_gate_notice(state);
            format!("Browser. No active tab. {notice}")
        }
    }
}

fn install_browser_accessibility(ctx: &egui::Context, rect: egui::Rect, state: &WebState) {
    let summary = browser_accessibility_summary(state);
    let _ = ctx.accesskit_node_builder(browser_accessibility_id(), |node| {
        node.set_role(egui::accesskit::Role::Status);
        node.set_live(egui::accesskit::Live::Polite);
        node.set_label("Browser status");
        node.set_value(summary);
        node.set_bounds(accesskit_rect(rect));
    });
}

fn install_browser_page_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    tab: &Tab,
    page_focused: bool,
) {
    let mut value = tab_accessibility_summary(tab);
    if page_focused {
        value.push_str(". Keyboard input is focused into the page canvas.");
    } else {
        value.push_str(". Click the page canvas to focus keyboard input.");
    }
    let _ = ctx.accesskit_node_builder(browser_page_accessibility_id(), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label("Browser page");
        node.set_value(value);
        node.set_bounds(accesskit_rect(rect));
        node.add_action(egui::accesskit::Action::Click);
    });
}

/// Translate one egui event into the page-local event forwarded to the helper.
///
/// Pointer positions are mapped from panel space into frame device pixels via the
/// shared [`map_pointer_to_frame`] (`rect` = the displayed image rect, `frame_size`
/// = the current decoded frame). Only pointer positions are rewritten; wheel, keys,
/// and text pass through unchanged (gated on page focus). A pointer that leaves the
/// image while the page is focused reports `PointerGone` so the page's hover clears.
fn browser_input_event(
    event: &egui::Event,
    rect: egui::Rect,
    frame_size: [usize; 2],
    browser_focused: bool,
) -> Option<egui::Event> {
    match event {
        egui::Event::PointerMoved(pos) => {
            if rect.contains(*pos) {
                Some(egui::Event::PointerMoved(map_pointer_to_frame(
                    *pos, rect, frame_size,
                )))
            } else if browser_focused {
                Some(egui::Event::PointerGone)
            } else {
                None
            }
        }
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers,
        } => {
            if rect.contains(*pos) || browser_focused {
                Some(egui::Event::PointerButton {
                    pos: map_pointer_to_frame(*pos, rect, frame_size),
                    button: *button,
                    pressed: *pressed,
                    modifiers: *modifiers,
                })
            } else {
                None
            }
        }
        egui::Event::MouseWheel {
            unit,
            delta,
            modifiers,
        } => browser_focused.then_some(egui::Event::MouseWheel {
            unit: *unit,
            delta: *delta,
            modifiers: *modifiers,
        }),
        egui::Event::Key {
            key,
            physical_key,
            pressed,
            repeat,
            modifiers,
        } => browser_focused.then_some(egui::Event::Key {
            key: *key,
            physical_key: *physical_key,
            pressed: *pressed,
            repeat: *repeat,
            modifiers: *modifiers,
        }),
        egui::Event::Text(text) => browser_focused.then_some(egui::Event::Text(text.clone())),
        _ => None,
    }
}

/// The honest "page crashed" body: a danger caption naming the reason plus a
/// Reload that respawns the tab (setting `respawn_requested`). Never a fake page.
fn crashed_body(ui: &mut egui::Ui, reason: String, respawn_requested: &mut bool) {
    centered(ui, |ui| {
        ui.label(
            RichText::new("This page crashed")
                .size(Style::HEADING)
                .color(Style::DANGER),
        );
        ui.add_space(Style::SP_S);
        if !reason.is_empty() {
            muted_note(ui, reason);
        }
        ui.add_space(Style::SP_M);
        if ui
            .add(egui::Button::new(
                RichText::new("\u{21BB} Reload").color(Style::TEXT),
            ))
            .clicked()
        {
            *respawn_requested = true;
        }
    });
}

/// The honest TLS/certificate-error interstitial: a full-content-area "sad
/// tab" painted instead of the page frame when the engine blocked a load for
/// a bad certificate (mirrors `crashed_body`'s pattern exactly). The engine
/// blocks by default and there is no "proceed anyway" affordance here — only
/// a way back. Returns `true` when "Back to safety" was clicked; the caller
/// decides go-back vs. close-tab from `can_back` (this fn has no session
/// access to act on it directly).
///
/// `TODO(security)`: an optional advanced "proceed anyway" override is a
/// deliberate follow-up — the blocking-by-default posture stays as-is here.
/// The full-page "unsafe site" interstitial for a safe-browsing block (mesh policy
/// source). A red warning naming the blocked host + a "Back to safety" button;
/// returns `true` when it's clicked. The blocked request was already dropped.
fn safe_browsing_interstitial_body(ui: &mut egui::Ui, url: &str) -> bool {
    let host = host_of(url).unwrap_or_else(|| url.trim().to_owned());
    let mut back = false;
    centered(ui, |ui| {
        ui.label(
            RichText::new("\u{26A0} Unsafe site blocked")
                .size(Style::HEADING)
                .color(Style::DANGER),
        );
        ui.add_space(Style::SP_M);
        ui.label(
            RichText::new(format!(
                "{host} is on the mesh safe-browsing blocklist. This page was not loaded."
            ))
            .color(Style::TEXT),
        );
        ui.add_space(Style::SP_M);
        if ui
            .add(egui::Button::new(
                RichText::new("Back to safety").color(Style::TEXT),
            ))
            .clicked()
        {
            back = true;
        }
    });
    back
}

/// Human label for an engine-neutral permission kind (matches
/// `EventMsg::PermissionRequest`: 0 geolocation, 1 notifications, 2 clipboard).
fn permission_kind_label(kind: u8) -> &'static str {
    match kind {
        0 => "know your location",
        1 => "show notifications",
        2 => "access the clipboard",
        _ => "use a device capability",
    }
}

/// A permission prompt bar shown atop the page when an origin requests a capability
/// ("<origin> wants to <capability>  [Allow] [Block]"). Returns `Some(true)` on
/// Allow, `Some(false)` on Block, `None` while undecided. Mirrors Chrome's origin
/// permission bar; a grant is remembered session-only by the caller.
fn permission_prompt_bar(ui: &mut egui::Ui, origin: &str, kind: u8) -> Option<bool> {
    let mut decision = None;
    egui::Frame::NONE
        .fill(Style::SURFACE_HI)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{origin} wants to {}", permission_kind_label(kind)))
                        .color(Style::TEXT),
                );
                if ui
                    .add(egui::Button::new(RichText::new("Allow").color(Style::TEXT)))
                    .clicked()
                {
                    decision = Some(true);
                }
                if ui
                    .add(egui::Button::new(RichText::new("Block").color(Style::TEXT)))
                    .clicked()
                {
                    decision = Some(false);
                }
            });
        });
    decision
}

/// A "Save password?" bar for an auto-captured login. Returns `Some(true)` to save,
/// `Some(false)` to dismiss, `None` while undecided. Mirrors the permission bar.
fn login_save_prompt_bar(ui: &mut egui::Ui, host: &str, username: &str) -> Option<bool> {
    let mut decision = None;
    egui::Frame::NONE
        .fill(Style::SURFACE_HI)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("Save login for {host} ({username})?"))
                        .color(Style::TEXT),
                );
                if ui
                    .add(egui::Button::new(RichText::new("Save").color(Style::TEXT)))
                    .clicked()
                {
                    decision = Some(true);
                }
                if ui
                    .add(egui::Button::new(
                        RichText::new("Not now").color(Style::TEXT),
                    ))
                    .clicked()
                {
                    decision = Some(false);
                }
            });
        });
    decision
}

fn cert_error_body(ui: &mut egui::Ui, err: &CertError, can_back: bool) -> bool {
    let mut back_to_safety = false;
    centered(ui, |ui| {
        ui.label(
            RichText::new("Your connection is not private")
                .size(Style::HEADING)
                .color(Style::DANGER),
        );
        ui.add_space(Style::SP_S);
        ui.label(
            RichText::new(cert_error_host(err))
                .size(Style::HEADING)
                .color(Style::TEXT),
        );
        ui.add_space(Style::SP_S);
        muted_note(ui, err.message.as_str());
        ui.add_space(Style::SP_XS);
        ui.label(
            RichText::new(format!("Error code {}", err.code))
                .size(Style::SMALL)
                .color(Style::TEXT_DIM),
        );
        ui.add_space(Style::SP_M);
        if ui
            .add(egui::Button::new(
                RichText::new("\u{2190} Back to safety").color(Style::TEXT),
            ))
            .clicked()
        {
            back_to_safety = true;
        }
        if !can_back {
            ui.add_space(Style::SP_XS);
            muted_note(ui, "No history to return to — this closes the tab.");
        }
    });
    back_to_safety
}

/// Render a daemon-owned private offline copy when the live page is unavailable.
fn cached_offline_body(
    ui: &mut egui::Ui,
    result: &BrowserOfflineCacheResult,
    unavailable_reason: Option<&str>,
) {
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Offline copy")
                        .size(Style::HEADING)
                        .color(Style::TEXT),
                );
                ui.label(
                    RichText::new(result.cache_id.chars().take(12).collect::<String>())
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("Copy")
                        .on_hover_text("Copy cached page text")
                        .clicked()
                    {
                        ui.ctx().copy_text(result.text.clone());
                    }
                });
            });
            if let Some(reason) = unavailable_reason
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
            {
                ui.add_space(Style::SP_XS);
                ui.label(
                    RichText::new(format!("Live page unavailable: {reason}"))
                        .size(Style::SMALL)
                        .color(Style::WARN),
                );
            }
            ui.add_space(Style::SP_XS);
            let page = if result.title.trim().is_empty() {
                result.url.as_str()
            } else {
                result.title.as_str()
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(page)
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} chars from {}",
                        result.text.chars().count(),
                        result.engine.label()
                    ))
                    .size(Style::SMALL)
                    .color(Style::TEXT_DIM),
                );
            });
            ui.add_space(Style::SP_S);
            egui::ScrollArea::vertical()
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(result.text.as_str())
                            .size(Style::SMALL)
                            .color(Style::TEXT),
                    );
                });
        });
}

/// The no-session `EmptyState` — an honest gated caption (or the NAMED live-path
/// notice when one is passed), never a placeholder page.
fn empty_body(ui: &mut egui::Ui, notice: Option<&str>) {
    centered(ui, |ui| {
        ui.label(
            RichText::new("Sandboxed browser")
                .size(Style::HEADING)
                .color(Style::TEXT),
        );
        ui.add_space(Style::SP_S);
        muted_note(
            ui,
            notice.unwrap_or(
                "The sandboxed Servo browser renders here in the shell. A live session \
                 attaches on a GPU seat (BOOKMARKS-5/6 live path is gated).",
            ),
        );
    });
}

/// Center `content` vertically + horizontally in the remaining body.
fn centered(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.5 - Style::SP_XL);
        content(ui);
    });
}

mod menubar;

#[cfg(test)]
mod tests {
    use super::*;
    use mde_egui::egui::{pos2, vec2, Rect};
    use mde_web_preview_client::{scm, testkit, wire};
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    /// A headless 960×640 shell body, mirroring the VDI + shell render tests.
    fn body_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(960.0, 640.0))),
            ..Default::default()
        }
    }

    /// One Ctrl(+Shift) key press as a frame's input — drives the tab-strip
    /// accelerators through the same event path a real seat produces.
    fn ctrl_key_input(key: egui::Key, shift: bool) -> egui::RawInput {
        let mut input = body_input();
        input.events = vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                shift,
                ..egui::Modifiers::default()
            },
        }];
        input
    }

    /// Drive one headless frame of `web_panel` on the CPU tessellation path (the
    /// same `Context::run` → `tessellate` the DRM runner drives, minus the GPU).
    fn run_panel(state: &mut WebState) -> bool {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        !prims.is_empty()
    }

    /// Run frames until the active tab uploads its texture (the fake helper's frame
    /// is already buffered, so this settles immediately).
    fn run_until_texture(state: &mut WebState) -> bool {
        run_until_texture_for(state, 50)
    }

    fn run_until_texture_for(state: &mut WebState, frames: usize) -> bool {
        for _ in 0..frames {
            run_panel(state);
            if state
                .tabs
                .get(state.active)
                .is_some_and(|t| t.texture.is_some())
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    fn run_panel_on_ctx(ctx: &egui::Context, state: &mut WebState, input: egui::RawInput) -> bool {
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        !prims.is_empty()
    }

    fn run_panel_output(
        ctx: &egui::Context,
        state: &mut WebState,
        input: egui::RawInput,
    ) -> egui::FullOutput {
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        })
    }

    fn accesskit_nodes(
        out: &egui::FullOutput,
    ) -> Vec<(egui::accesskit::NodeId, egui::accesskit::Node)> {
        out.platform_output
            .accesskit_update
            .as_ref()
            .expect("accesskit update")
            .nodes
            .clone()
    }

    #[test]
    fn curated_userscript_bundle_contains_the_first_site_fixups() {
        let bundle = curated_userscript_bundle(&[]);
        assert_eq!(CURATED_USERSCRIPT_COUNT, 100);
        assert_eq!(CURATED_USERSCRIPTS.len(), CURATED_USERSCRIPT_COUNT);
        for needle in [
            "youtube-focus",
            "npr-reader",
            "spotify-quiet",
            "wikipedia-readable",
            "nytimes-clean-reader",
            "github-readable",
            "amazon-clean-shop",
            "allrecipes-clean-recipe",
            "coursera-readable",
            "mde-browser-userscript-style",
            "__mdeBrowserUserScriptsObserver",
        ] {
            assert!(
                bundle.contains(needle),
                "missing userscript payload: {needle}"
            );
        }
    }

    #[test]
    fn curated_userscript_bundle_folds_in_user_site_styles_and_skips_blanks() {
        let styles = vec![
            UserSiteStyle {
                host: "www.Example.com".into(),
                css: "body{background:#000}".into(),
            },
            UserSiteStyle {
                host: "  ".into(),
                css: "x{y:z}".into(),
            }, // blank host → skipped
            UserSiteStyle {
                host: "site.test".into(),
                css: "   ".into(),
            }, // blank css → skipped
        ];
        let bundle = curated_userscript_bundle(&styles);
        // The user rule renders with a normalized (www-stripped, lowercased) host.
        assert!(bundle.contains("example.com"));
        assert!(bundle.contains("body{background:#000}"));
        assert!(bundle.contains("user:0"));
        // Blank host / blank CSS entries are skipped.
        assert!(!bundle.contains("user:1"));
        assert!(!bundle.contains("user:2"));
    }

    #[test]
    fn browser_page_exports_accesskit_status_and_clickable_page_region() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        state.tabs[state.active].force_dark = true;
        state.tabs[state.active].reader_mode = true;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);

        let out = run_panel_output(&ctx, &mut state, body_input());
        let nodes = accesskit_nodes(&out);
        let browser = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Browser status"))
            .expect("browser status accesskit node");
        assert_eq!(browser.role(), egui::accesskit::Role::Status);
        assert_eq!(browser.live(), Some(egui::accesskit::Live::Polite));
        let browser_value = browser.value().expect("browser status value");
        assert!(browser_value.contains("Active tab 1 of 1"));
        assert!(browser_value.contains("Example"));
        assert!(browser_value.contains("https://example.test/"));
        assert!(browser_value.contains("force dark"));
        assert!(browser_value.contains("reader mode"));

        let page = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Browser page"))
            .expect("browser page accesskit node");
        assert_eq!(page.role(), egui::accesskit::Role::Button);
        let page_value = page.value().expect("browser page value");
        assert!(
            !page_value.contains("CEF"),
            "test session defaults to Servo"
        );
        assert!(page_value.contains("Click the page canvas to focus keyboard input"));
    }

    fn write_helper_event(stream: &UnixStream, msg: &mde_web_preview_client::EventMsg) {
        let mut stream = stream;
        stream
            .write_all(&wire::frame(&msg.encode()))
            .expect("write helper event");
    }

    /// A bare socketpair session with no shm/frame plumbing — enough to drive
    /// wire-level events (favicons, nav, …) without a fake helper thread.
    /// Mirrors `the_ad_filter_blocked_count_surfaces_on_the_active_tab`'s recipe;
    /// testkit's `connect()` doesn't expose its peer socket for manual events.
    fn raw_session_pair() -> (WebSession, UnixStream) {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let session = WebSession::from_stream(shell, None).expect("session");
        (session, helper)
    }

    /// Push an `EventMsg::Favicon` carrying `png_bytes` to a raw session's peer.
    /// Caller polls the session afterward to fold it in.
    fn send_favicon(peer: &UnixStream, png_bytes: &[u8]) {
        write_helper_event(
            peer,
            &mde_web_preview_client::EventMsg::Favicon {
                png: png_bytes.to_vec(),
            },
        );
    }

    /// A raw session that has already polled in one favicon's PNG bytes.
    fn session_with_favicon(png_bytes: &[u8]) -> WebSession {
        let (mut session, peer) = raw_session_pair();
        send_favicon(&peer, png_bytes);
        session.poll();
        session
    }

    /// Push an `EventMsg::CertError` to a raw session's peer. Caller polls the
    /// session afterward to fold it in.
    fn send_cert_error(peer: &UnixStream, url: &str, code: i32, message: &str) {
        write_helper_event(
            peer,
            &mde_web_preview_client::EventMsg::CertError {
                url: url.to_owned(),
                code,
                message: message.to_owned(),
            },
        );
    }

    /// A raw session that has already polled in one blocked-navigation
    /// certificate error.
    fn session_with_cert_error(url: &str, code: i32, message: &str) -> WebSession {
        let (mut session, peer) = raw_session_pair();
        send_cert_error(&peer, url, code, message);
        session.poll();
        session
    }

    fn live_page_session() -> (
        WebSession,
        UnixStream,
        mde_web_preview_client::testkit::FrameWriter,
    ) {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let writer =
            testkit::FrameWriter::create(testkit::FAKE_W, testkit::FAKE_H).expect("frame writer");
        writer
            .emit(
                testkit::FAKE_W,
                testkit::FAKE_H,
                mde_web_preview_client::PixelFormat::Rgba8,
                &testkit::gradient(testkit::FAKE_W, testkit::FAKE_H),
            )
            .expect("emit frame");
        scm::send_frame_with_fd(
            &helper,
            &mde_web_preview_client::EventMsg::AttachFrame.encode(),
            writer.raw_fd(),
        )
        .expect("attach frame");
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://example.test/".to_owned(),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::Title("Example".to_owned()),
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PaintReady {
                seq: writer.sequence(),
            },
        );
        helper.set_nonblocking(true).expect("nonblocking helper");
        (
            WebSession::from_stream(shell, None).expect("session"),
            helper,
            writer,
        )
    }

    fn drain_control_messages(stream: &UnixStream) -> Vec<mde_web_preview_client::ControlMsg> {
        let mut rbuf = Vec::new();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            match scm::recv(stream).expect("recv controls") {
                scm::RecvOutcome::Data { bytes, .. } => {
                    rbuf.extend_from_slice(&bytes);
                    while let Some(payload) = wire::take_frame(&mut rbuf).expect("control frame") {
                        out.push(
                            mde_web_preview_client::ControlMsg::decode(&payload)
                                .expect("control decode"),
                        );
                    }
                }
                scm::RecvOutcome::WouldBlock => {
                    if !out.is_empty() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                scm::RecvOutcome::Eof => break,
            }
        }
        out
    }

    fn wait_for_fresh_frame(state: &mut WebState) -> bool {
        for _ in 0..100 {
            let Some(tab) = state.tabs.get_mut(state.active) else {
                return false;
            };
            tab.session.poll();
            if tab.session.take_frame().is_some() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn browser_capture_filename_prefers_host_and_sanitizes() {
        assert_eq!(
            capture_filename_for("https://Example.COM/some/path", "Ignored", 123),
            "mde-browser-123-example-com.png"
        );
        assert_eq!(
            capture_filename_for("about:blank", "News: Top Stories / Today", 456),
            "mde-browser-456-news-top-stories-today.png"
        );
        assert_eq!(
            capture_full_page_filename_for("https://Example.COM/some/path", "Ignored", 678),
            "mde-browser-full-page-678-example-com.png"
        );
        assert_eq!(
            capture_mhtml_filename_for("https://Example.COM/some/path", "Ignored", 679),
            "mde-browser-679-example-com.mhtml"
        );
        assert_eq!(
            capture_annotated_filename_for("https://Example.COM/some/path", "Ignored", 789),
            "mde-browser-annotated-789-example-com.png"
        );
        assert_eq!(
            capture_callout_filename_for("https://Example.COM/some/path", "Ignored", 888),
            "mde-browser-callout-888-example-com.png"
        );
        assert_eq!(
            capture_freehand_filename_for("https://Example.COM/some/path", "Ignored", 889),
            "mde-browser-freehand-889-example-com.png"
        );
        assert_eq!(
            pdf_filename_for("https://Example.COM/some/path", "Ignored", 999),
            "mde-browser-999-example-com.pdf"
        );
    }

    #[test]
    fn browser_capture_writes_latest_frame_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            run_until_texture(&mut state),
            "the fake helper frame should upload before capture"
        );
        assert!(
            state.active_tab_has_frame(),
            "capture is gated on a retained helper frame"
        );

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_viewport_to_dir(dir.path())
            .expect("capture writes PNG");
        assert_eq!(path.parent(), Some(dir.path()));
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("example-test") && name.ends_with(".png")),
            "capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize],
            "capture preserves the helper viewport dimensions"
        );
    }

    #[test]
    fn browser_full_page_capture_writes_named_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_full_page_to_dir(dir.path())
            .expect("full-page capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-full-page-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "full-page capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize],
            "full-page capture preserves the current helper surface until stitching lands"
        );
    }

    #[test]
    fn browser_mhtml_capture_writes_related_archive() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_mhtml_to_dir(dir.path())
            .expect("mhtml capture writes archive");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-")
                    && name.contains("example-test")
                    && name.ends_with(".mhtml")),
            "mhtml capture filename should include the current host: {}",
            path.display()
        );
        let archive = std::fs::read_to_string(&path).expect("read mhtml");
        assert!(archive.contains("Content-Type: multipart/related"));
        assert!(archive.contains("Content-Type: text/html; charset=\"utf-8\""));
        assert!(archive.contains("Content-Type: image/png"));
        assert!(archive.contains("Content-Transfer-Encoding: base64"));
        assert!(archive.contains("https://example.test/"));
        assert!(archive.contains("mde-browser-capture.png"));
        assert!(
            archive.contains("iVBORw0KGgo"),
            "the embedded related part should contain a base64 PNG payload"
        );
    }

    #[test]
    fn mhtml_capture_escapes_page_metadata() {
        let archive = mhtml_capture_document(
            "https://example.test/?q=<tag>&x=1",
            "A <Title> & \"Quote\"",
            42,
            b"not a real png for structure testing",
        );
        let archive = String::from_utf8(archive).expect("mhtml is utf8");
        assert!(archive.contains("A &lt;Title&gt; &amp; &quot;Quote&quot;"));
        assert!(archive.contains("https://example.test/?q=&lt;tag&gt;&amp;x=1"));
        assert!(!archive.contains("<Title>"));
    }

    #[test]
    fn browser_annotated_capture_writes_captioned_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_annotated_viewport_to_dir(dir.path())
            .expect("annotated capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-annotated-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "annotated capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [
                testkit::FAKE_W as usize,
                testkit::FAKE_H as usize + ANNOTATION_BAR_HEIGHT
            ],
            "annotated capture appends a visible caption band"
        );
        let frame_pixels = (testkit::FAKE_W * testkit::FAKE_H) as usize;
        assert!(
            image.pixels[frame_pixels..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT),
            "caption band should contain rendered annotation text"
        );
    }

    #[test]
    fn browser_callout_capture_writes_annotated_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_callout_viewport_to_dir(dir.path())
            .expect("callout capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-callout-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "callout capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [
                testkit::FAKE_W as usize,
                testkit::FAKE_H as usize + ANNOTATION_BAR_HEIGHT
            ],
            "callout capture appends a visible caption band"
        );
        let frame_pixels = (testkit::FAKE_W * testkit::FAKE_H) as usize;
        assert!(
            image.pixels[frame_pixels..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "callout capture should render a callout label into the caption band"
        );
    }

    #[test]
    fn browser_freehand_capture_writes_annotated_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_freehand_viewport_to_dir(dir.path())
            .expect("freehand capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-freehand-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "freehand capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [
                testkit::FAKE_W as usize,
                testkit::FAKE_H as usize + ANNOTATION_BAR_HEIGHT
            ],
            "freehand capture appends a visible caption band"
        );
        let frame_pixels = (testkit::FAKE_W * testkit::FAKE_H) as usize;
        assert!(
            image.pixels[frame_pixels..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "freehand capture should render a freehand label into the caption band"
        );
    }

    #[test]
    fn annotated_capture_preserves_frame_and_adds_caption_band() {
        let mut img = egui::ColorImage::new([32, 4], egui::Color32::RED);
        img.pixels[3] = egui::Color32::BLUE;

        let annotated = annotate_capture_image(&img, "Example | https://example.test | 123")
            .expect("valid annotation");

        assert_eq!(annotated.size, [32, 4 + ANNOTATION_BAR_HEIGHT]);
        assert_eq!(&annotated.pixels[..img.pixels.len()], &img.pixels[..]);
        assert!(
            annotated.pixels[img.pixels.len()..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT),
            "caption text should be painted into the appended band"
        );
    }

    #[test]
    fn callout_capture_draws_overlay_and_preserves_frame_area() {
        let img = egui::ColorImage::new([64, 48], egui::Color32::RED);

        let annotated =
            annotate_callout_capture_image(&img, "Example | https://example.test | 123")
                .expect("valid callout annotation");

        assert_eq!(annotated.size, [64, 48 + ANNOTATION_BAR_HEIGHT]);
        assert_eq!(
            annotated.pixels[0], img.pixels[0],
            "unannotated corners of the captured frame remain intact"
        );
        assert!(
            annotated.pixels[..(64 * 48)]
                .iter()
                .any(|pixel| *pixel == Style::ACCENT),
            "callout overlay should paint an accent rectangle or leader line"
        );
        assert!(
            annotated.pixels[(64 * 48)..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "callout label should be painted into the appended band"
        );
    }

    #[test]
    fn freehand_capture_draws_stroke_and_preserves_frame_area() {
        let img = egui::ColorImage::new([64, 48], egui::Color32::RED);

        let annotated =
            annotate_freehand_capture_image(&img, "Example | https://example.test | 123")
                .expect("valid freehand annotation");

        assert_eq!(annotated.size, [64, 48 + ANNOTATION_BAR_HEIGHT]);
        assert_eq!(
            annotated.pixels[0], img.pixels[0],
            "unannotated corners of the captured frame remain intact"
        );
        assert!(
            annotated.pixels[..(64 * 48)]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "freehand overlay should paint a visible white stroke"
        );
        assert!(
            annotated.pixels[(64 * 48)..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "freehand label should be painted into the appended band"
        );
    }

    #[test]
    fn browser_region_capture_crops_latest_frame_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_region_to_dir(
                dir.path(),
                PixelRegion {
                    x: 1,
                    y: 1,
                    width: 3,
                    height: 2,
                },
            )
            .expect("region capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-region-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "region capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(image.size, [3, 2]);
    }

    #[test]
    fn browser_region_crop_validates_and_preserves_pixels() {
        let mut img = egui::ColorImage::new([4, 3], egui::Color32::TRANSPARENT);
        for y in 0..3 {
            for x in 0..4 {
                // Twelve distinct sentinel pixels (a WHITE ramp) so the crop's
                // positional preservation is provable without minting a colour.
                img.pixels[y * 4 + x] =
                    egui::Color32::WHITE.gamma_multiply((y * 4 + x + 1) as f32 / 12.0);
            }
        }

        let cropped = crop_color_image(
            &img,
            PixelRegion {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            },
        )
        .expect("valid crop");

        assert_eq!(cropped.size, [2, 2]);
        assert_eq!(cropped.pixels[0], img.pixels[5]);
        assert_eq!(cropped.pixels[1], img.pixels[6]);
        assert_eq!(cropped.pixels[2], img.pixels[9]);
        assert_eq!(cropped.pixels[3], img.pixels[10]);
        assert!(
            crop_color_image(
                &img,
                PixelRegion {
                    x: 3,
                    y: 2,
                    width: 2,
                    height: 1,
                },
            )
            .is_err(),
            "out-of-bounds regions are rejected"
        );
    }

    #[test]
    fn pdf_completion_events_update_the_browser_notice() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PdfSaved {
                path: "/tmp/mde-page.pdf".to_owned(),
                ok: true,
            },
        );
        run_panel(&mut state);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("PDF saved /tmp/mde-page.pdf")
        );

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PdfSaved {
                path: "/tmp/mde-page.pdf".to_owned(),
                ok: false,
            },
        );
        run_panel(&mut state);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("PDF failed /tmp/mde-page.pdf")
        );
    }

    #[test]
    fn saved_pdf_opens_in_a_cef_viewer_tab() {
        let dir = tempfile::tempdir().expect("pdf dir");
        let path = dir.path().join("report one.pdf");
        std::fs::write(&path, b"%PDF-1.7\n% viewer fixture\n").expect("pdf fixture");
        let path_text = path.to_string_lossy().into_owned();
        let mut state = WebState::default();

        assert_eq!(
            state.handle_pdf_event(path_text.clone(), true),
            format!("PDF saved {path_text}")
        );
        state.open_last_saved_pdf();

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: format!("file://{}", path_text.replace(' ', "%20")),
            })
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Opening PDF in CEF viewer")
        );
    }

    #[test]
    fn pdf_viewer_refuses_missing_or_non_pdf_output() {
        let dir = tempfile::tempdir().expect("pdf dir");
        let path = dir.path().join("not-pdf.pdf");
        std::fs::write(&path, b"not a pdf\n").expect("bad fixture");
        let mut state = WebState::default();
        state.last_saved_pdf = Some(SavedPdf {
            path: path.clone(),
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
        });

        state.open_last_saved_pdf();

        assert_eq!(state.take_open_request(), None);
        assert!(
            state
                .capture_notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("PDF viewer failed:")),
            "viewer should explain the refused file: {:?}",
            state.capture_notice
        );
    }

    #[test]
    fn browser_body_input_is_localized_and_keyboard_is_focus_gated() {
        let rect = Rect::from_min_size(pos2(20.0, 40.0), vec2(320.0, 200.0));
        // Frame device size == rect points size (a 1:1 seat), so the mapped position
        // equals the panel-local offset — pins the transform's identity case.
        let frame = [320usize, 200usize];

        let moved = browser_input_event(
            &egui::Event::PointerMoved(pos2(70.0, 90.0)),
            rect,
            frame,
            false,
        )
        .expect("pointer inside page");
        assert_eq!(moved, egui::Event::PointerMoved(pos2(50.0, 50.0)));

        let key = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        assert_eq!(
            browser_input_event(&key, rect, frame, false),
            None,
            "address-bar/chrome keystrokes must not leak into the page"
        );
        assert_eq!(
            browser_input_event(&key, rect, frame, true),
            Some(key),
            "click-focused page canvas receives keyboard events"
        );
        assert_eq!(
            browser_input_event(&egui::Event::Text("mesh".to_owned()), rect, frame, true),
            Some(egui::Event::Text("mesh".to_owned())),
            "committed text reaches the focused page canvas"
        );
    }

    // ─────────────── browser-1: pointer transform + viewport resize ──────────────
    //
    // The bug forwarded `(pointer - image_rect.min) * pixels_per_point` against a
    // FIXED 1280×800 frame, so clicks missed on every seat whose displayed rect
    // wasn't exactly 1280×800 device px. These pin the shared transform both the
    // live-input and region-capture paths now flow through.

    #[test]
    fn map_pointer_to_frame_scales_panel_space_into_frame_pixels() {
        // A non-zero origin (tab strip + nav chrome above/left) and a frame TWICE
        // the panel per axis — the exact non-1280×800 shape the fixed frame ignored.
        let rect = Rect::from_min_size(pos2(100.0, 40.0), vec2(800.0, 600.0));
        let frame = [1600usize, 1200usize];
        // Image top-left → frame origin.
        assert_eq!(
            map_pointer_to_frame(pos2(100.0, 40.0), rect, frame),
            pos2(0.0, 0.0)
        );
        // Image centre → frame centre.
        assert_eq!(
            map_pointer_to_frame(pos2(500.0, 340.0), rect, frame),
            pos2(800.0, 600.0)
        );
        // A quarter across the image → a quarter across the frame.
        assert_eq!(
            map_pointer_to_frame(pos2(300.0, 190.0), rect, frame),
            pos2(400.0, 300.0)
        );
        // Image bottom-right → the frame's far edge.
        assert_eq!(
            map_pointer_to_frame(pos2(900.0, 640.0), rect, frame),
            pos2(1600.0, 1200.0)
        );
    }

    #[test]
    fn map_pointer_to_frame_clamps_outside_the_image_and_survives_zero_size() {
        let rect = Rect::from_min_size(pos2(50.0, 30.0), vec2(500.0, 400.0));
        let frame = [250usize, 200usize];
        // Above/left of the image clamps to the origin (never negative).
        assert_eq!(
            map_pointer_to_frame(pos2(0.0, 0.0), rect, frame),
            pos2(0.0, 0.0)
        );
        // Far below/right clamps to the frame's far edge (never runs off).
        assert_eq!(
            map_pointer_to_frame(pos2(9000.0, 9000.0), rect, frame),
            pos2(250.0, 200.0)
        );
        // A degenerate zero-size image must not divide by zero.
        let zero = Rect::from_min_size(pos2(10.0, 10.0), vec2(0.0, 0.0));
        assert_eq!(
            map_pointer_to_frame(pos2(50.0, 50.0), zero, [640, 480]),
            pos2(0.0, 0.0)
        );
    }

    #[test]
    fn browser_click_maps_through_the_shared_frame_transform() {
        // A 4K frame displayed into a smaller, offset panel (the downscale case the
        // fixed-1280×800 path got wrong). A click at the panel centre must land at
        // the FRAME centre — proving the live path routes through the transform, not
        // a `pos * ppp` scale — and must MATCH what the region-capture path computes
        // for the same pointer (the dedup the review asked for: no divergence).
        let rect = Rect::from_min_size(pos2(64.0, 128.0), vec2(960.0, 540.0));
        let frame = [3840usize, 2160usize];
        let centre = pos2(64.0 + 480.0, 128.0 + 270.0);
        let ev = egui::Event::PointerButton {
            pos: centre,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        };
        match browser_input_event(&ev, rect, frame, true).expect("focused click forwards") {
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                ..
            } => {
                assert_eq!(pos, pos2(1920.0, 1080.0), "click lands at the frame centre");
                // Region-capture's `pointer_to_frame` IS `map_pointer_to_frame`, so
                // both paths agree on the same pointer — one shared transform.
                assert_eq!(pos, map_pointer_to_frame(centre, rect, frame));
                assert_eq!(button, egui::PointerButton::Primary);
                assert!(pressed);
            }
            other => panic!("expected PointerButton, got {other:?}"),
        }
        // A focused pointer leaving the image reports PointerGone (hover clears).
        assert_eq!(
            browser_input_event(
                &egui::Event::PointerMoved(pos2(0.0, 0.0)),
                rect,
                frame,
                true
            ),
            Some(egui::Event::PointerGone)
        );
    }

    #[test]
    fn frame_target_device_px_scales_by_ppp_and_clamps() {
        let rect = Rect::from_min_size(pos2(8.0, 8.0), vec2(1000.0, 500.0));
        assert_eq!(frame_target_device_px(rect, 1.0), (1000, 500));
        // A HiDPI seat scales the panel into more device pixels.
        assert_eq!(frame_target_device_px(rect, 2.0), (2000, 1000));
        // Beyond the channel ceiling clamps (an oversized panel at 2×).
        let huge = Rect::from_min_size(pos2(0.0, 0.0), vec2(3000.0, 3000.0));
        assert_eq!(
            frame_target_device_px(huge, 2.0),
            (MAX_CHANNEL_DIM, MAX_CHANNEL_DIM)
        );
        // Never smaller than 1×1.
        let z = Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 0.0));
        assert_eq!(frame_target_device_px(z, 1.0), (1, 1));
    }

    #[test]
    fn viewport_resizer_debounces_a_changed_size_and_ignores_no_change() {
        let mut r = ViewportResizer::default();
        let t0 = Instant::now();
        let d = Duration::from_millis(150);
        // First sighting of a new size: still settling, no resize yet.
        assert_eq!(r.observe((1200, 700), t0, d), None);
        assert!(r.is_settling());
        // Held steady but before the debounce elapses: still nothing.
        assert_eq!(
            r.observe((1200, 700), t0 + Duration::from_millis(100), d),
            None
        );
        // Settled (held ≥ debounce): committed exactly once.
        assert_eq!(
            r.observe((1200, 700), t0 + Duration::from_millis(150), d),
            Some((1200, 700))
        );
        assert!(!r.is_settling());
        // The SAME size again is a no-op — an unchanged panel never re-resizes.
        assert_eq!(r.observe((1200, 700), t0 + Duration::from_secs(1), d), None);
    }

    #[test]
    fn viewport_resizer_restarts_the_clock_while_the_size_keeps_changing() {
        let mut r = ViewportResizer::default();
        let t0 = Instant::now();
        let d = Duration::from_millis(150);
        // A drag moves through many sizes: each new candidate restarts the debounce,
        // so no resize fires mid-drag even past the first candidate's deadline.
        assert_eq!(r.observe((1000, 600), t0, d), None);
        assert_eq!(
            r.observe((1010, 600), t0 + Duration::from_millis(100), d),
            None
        );
        assert_eq!(
            r.observe((1020, 600), t0 + Duration::from_millis(200), d),
            None
        );
        assert!(r.is_settling());
        // The drag settles on the final size; once THAT holds for the debounce it
        // commits — a single settled resize for the whole drag.
        let settled = t0 + Duration::from_millis(200);
        assert_eq!(r.observe((1020, 600), settled + d, d), Some((1020, 600)));
    }

    #[test]
    fn viewport_resizer_cancels_a_pending_change_that_reverts() {
        let mut r = ViewportResizer::default();
        let t0 = Instant::now();
        let d = Duration::from_millis(150);
        r.observe((800, 600), t0, d);
        assert_eq!(r.observe((800, 600), t0 + d, d), Some((800, 600)));
        // A transient candidate appears...
        assert_eq!(
            r.observe((801, 600), t0 + d + Duration::from_millis(10), d),
            None
        );
        assert!(r.is_settling());
        // ...but reverts to the committed size before settling: pending cancels, so
        // no spurious resize.
        assert_eq!(
            r.observe((800, 600), t0 + d + Duration::from_millis(20), d),
            None
        );
        assert!(!r.is_settling());
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn a_live_spawn_pre_sizes_the_channel_to_the_seat() {
        use std::cell::Cell;
        // A seat reporting a 1920×1080 screen: the spawn must pre-size the channel to
        // it (item 3), NOT the fixed 1280×800 that would silently drop an enlarged
        // paint on a bigger panel.
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1920.0, 1080.0))),
            ..Default::default()
        };
        let _ = ctx.run(input, |_| {});
        let ppp = ctx.pixels_per_point();

        let mut state = WebState::default();
        state.note_seat_px(&ctx);
        let seen = Cell::new((0u32, 0u32));
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            std::env::current_exe().expect("test exe"),
            |spec| {
                seen.set((spec.width, spec.height));
                let (session, _helper) = testkit::connect()?;
                Ok(session)
            },
        );
        let expect = ((1920.0 * ppp).round() as u32, (1080.0 * ppp).round() as u32);
        assert_eq!(
            seen.get(),
            expect,
            "the spawn pre-sizes the channel to the seat, not a fixed 1280×800"
        );
        assert_ne!(seen.get(), (INIT_W, INIT_H));
    }

    #[test]
    fn browser_panel_click_focuses_page_and_sends_keyboard_input_to_helper() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(
            run_panel_on_ctx(&ctx, &mut state, body_input()),
            "the live page frame must upload before body input can be painted"
        );

        let page_point = pos2(480.0, 420.0);
        let mut click_input = body_input();
        click_input.events = vec![
            egui::Event::PointerMoved(page_point),
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, click_input));
        assert!(
            state.tabs[0].page_focused,
            "clicking the rendered page must latch page keyboard focus"
        );
        let mut key_input = body_input();
        key_input.events = vec![
            egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::Text("mesh".to_owned()),
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, key_input));

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::PointerButton { pressed: true, .. }
                )
            )),
            "clicking the Browser body must send a page pointer press: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Key {
                        key: mde_web_preview_client::wire::KeyCode::A,
                        pressed: true,
                        ..
                    }
                )
            )),
            "a focused Browser body must forward key input: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Text(text)
                ) if text == "mesh"
            )),
            "a focused Browser body must forward committed text: {controls:?}"
        );
    }

    #[test]
    fn a_focused_page_forwards_ime_composition_to_the_helper() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        // Focus the page body.
        let page_point = pos2(480.0, 420.0);
        let mut click = body_input();
        click.events = vec![
            egui::Event::PointerMoved(page_point),
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, click));
        assert!(state.tabs[0].page_focused);

        // A preedit then a commit: the page must receive IME composition controls,
        // NOT a Text input (composition is a distinct browser-host path).
        let mut ime = body_input();
        ime.events = vec![
            egui::Event::Ime(egui::ImeEvent::Preedit("\u{4f60}".to_owned())),
            egui::Event::Ime(egui::ImeEvent::Commit("\u{4f60}\u{597d}".to_owned())),
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, ime));

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ImeSetComposition { text } if text == "\u{4f60}"
            )),
            "a preedit must forward ImeSetComposition: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ImeCommitText { text } if text == "\u{4f60}\u{597d}"
            )),
            "a commit must forward ImeCommitText: {controls:?}"
        );
    }

    #[test]
    fn page_zoom_and_find_actions_send_helper_controls() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();

        menubar::apply(&ctx, &mut state, menubar::MenuAction::ZoomIn);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ZoomOut);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::OpenFind);
        assert!(
            state.find_open,
            "OpenFind keeps the compact find chrome visible until close"
        );
        state.find_query = "mesh".to_owned();
        state.submit_find(false);
        state.submit_find(true);
        state.close_find_bar();
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleAudioMute);
        assert!(state.tabs[state.active].muted);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleAudioMute);
        assert!(!state.tabs[state.active].muted);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleForceDark);
        assert!(state.tabs[state.active].force_dark);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleForceDark);
        assert!(!state.tabs[state.active].force_dark);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleReaderMode);
        assert!(state.tabs[state.active].reader_mode);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleReaderMode);
        assert!(!state.tabs[state.active].reader_mode);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleUserScripts);
        assert!(state.tabs[state.active].user_scripts);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleUserScripts);
        assert!(!state.tabs[state.active].user_scripts);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleUserAgent);
        assert_eq!(
            state.tabs[state.active].user_agent,
            UserAgentOverride::DesktopChrome
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleUserAgent);
        assert_eq!(
            state.tabs[state.active].user_agent,
            UserAgentOverride::AndroidChrome
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleDeviceProfile);
        assert_eq!(
            state.tabs[state.active].device_profile,
            DeviceProfile::Phone
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleDeviceProfile);
        assert_eq!(
            state.tabs[state.active].device_profile,
            DeviceProfile::Tablet
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CheckSpelling);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: reading page text")
        );
        let print_dir = tempfile::tempdir().expect("print spool dir");
        let print_path = state
            .queue_active_page_cups_print_to_dir(print_dir.path())
            .expect("cups print path queued");
        let print_path_text = print_path.to_string_lossy().into_owned();
        menubar::apply(&ctx, &mut state, menubar::MenuAction::PrintPage);
        let pdf_dir = tempfile::tempdir().expect("pdf output dir");
        let pdf_path = state
            .save_active_page_pdf_to_dir(pdf_dir.path())
            .expect("pdf path queued");
        let pdf_path_text = pdf_path.to_string_lossy().into_owned();

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetZoom { percent: 110 }
            )),
            "zoom in must send a helper zoom control: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetZoom { percent: 100 }
            )),
            "zoom out must restore a helper zoom control: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::FindInPage {
                    query,
                    backwards: false,
                    ..
                } if query == "mesh"
            )),
            "forward find must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::FindInPage {
                    query,
                    backwards: true,
                    ..
                } if query == "mesh"
            )),
            "backward find must reach the helper: {controls:?}"
        );
        assert!(
            controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::ClearFind)),
            "closing find must clear the helper selection: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetAudioMuted { muted: true }
            )),
            "muting the tab must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetAudioMuted { muted: false }
            )),
            "unmuting the tab must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetForceDark { enabled: true }
            )),
            "enabling force-dark must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetForceDark { enabled: false }
            )),
            "disabling force-dark must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetReaderMode { enabled: true }
            )),
            "enabling reader mode must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetReaderMode { enabled: false }
            )),
            "disabling reader mode must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserScripts {
                    enabled: true,
                    bundle,
                } if bundle.contains("youtube-focus")
                    && bundle.contains("npr-reader")
                    && bundle.contains("spotify-quiet")
            )),
            "enabling curated userscripts must reach the helper with the bundled library: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserScripts {
                    enabled: false,
                    bundle,
                } if bundle.is_empty()
            )),
            "disabling curated userscripts must clear the helper bundle: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserAgent { user_agent }
                    if user_agent.contains("X11; Linux x86_64")
                        && user_agent.contains("Chrome/126")
            )),
            "desktop UA override must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserAgent { user_agent }
                    if user_agent.contains("Android 14")
                        && user_agent.contains("Mobile Safari")
            )),
            "mobile UA override must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetDeviceProfile {
                    profile,
                    width: 390,
                    height: 844,
                    scale_percent: 300,
                    touch: true,
                } if profile == "phone"
            )),
            "phone device profile must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetDeviceProfile {
                    profile,
                    width: 820,
                    height: 1180,
                    scale_percent: 200,
                    touch: true,
                } if profile == "tablet"
            )),
            "tablet device profile must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "spellcheck must request bounded page text from the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SavePdf { path }
                    if path == &print_path_text
            )),
            "CUPS print must request a helper PDF before lp submission: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SavePdf { path } if path == &pdf_path_text
            )),
            "save-as-PDF must reach the helper with the chosen path: {controls:?}"
        );
    }

    #[test]
    fn page_context_menu_actions_send_native_helper_controls() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_page_context_action(state.active, PageContextAction::Reload);
        state.apply_page_context_action(state.active, PageContextAction::Edit(EditCommand::Copy));
        state.apply_page_context_action(
            state.active,
            PageContextAction::Edit(EditCommand::SelectAll),
        );

        let controls = drain_control_messages(&helper);
        assert!(
            controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::Reload)),
            "context-menu Reload must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::EditCommand {
                    command: EditCommand::Copy
                }
            )),
            "context-menu Copy must send a native edit command: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::EditCommand {
                    command: EditCommand::SelectAll
                }
            )),
            "context-menu Select-all must send a native edit command: {controls:?}"
        );
    }

    #[test]
    fn spellcheck_notice_summarizes_results_without_hunspell() {
        assert_eq!(
            spellcheck_notice(Ok(Vec::new())),
            "Spelling: no misspellings found"
        );
        assert_eq!(
            spellcheck_notice(Ok(vec![SpellMiss {
                chars: 0..5,
                word: "wrold".to_owned(),
                suggestions: vec!["world".to_owned()],
            }])),
            "Spelling: 1 possible misspelling; first: wrold -> world"
        );
        assert_eq!(
            spellcheck_notice(Err("hunspell not installed".to_owned())),
            "Spelling unavailable: hunspell not installed"
        );
    }

    #[test]
    fn browser_spellcheck_result_model_keeps_misses_and_copy_text() {
        let misses = vec![
            SpellMiss {
                chars: 0..5,
                word: "wrold".to_owned(),
                suggestions: vec!["world".to_owned(), "would".to_owned()],
            },
            SpellMiss {
                chars: 12..18,
                word: "msh".to_owned(),
                suggestions: Vec::new(),
            },
        ];
        let result = BrowserSpellcheckResult::from_result(3, Ok(misses.clone()));
        assert!(result.is_visible());
        assert_eq!(result.tab_index, 3);
        assert_eq!(result.summary(), "2 possible misspellings");
        assert_eq!(result.misses, misses);
        assert_eq!(
            spellcheck_results_text(&result.misses),
            "wrold [0..5]: world, would\nmsh [12..18]: no suggestions"
        );

        let unavailable =
            BrowserSpellcheckResult::from_result(4, Err("hunspell not installed".to_owned()));
        assert!(unavailable.is_visible());
        assert_eq!(unavailable.tab_index, 4);
        assert_eq!(
            unavailable.summary(),
            "Spellcheck unavailable: hunspell not installed"
        );
    }

    #[test]
    fn spellcheck_highlight_words_are_bounded_and_deduped() {
        let words = spellcheck_highlight_words(&[
            SpellMiss {
                chars: 0..5,
                word: " wrold ".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 7..12,
                word: "wrold".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 13..14,
                word: "x".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 15..20,
                word: "msh".to_owned(),
                suggestions: Vec::new(),
            },
        ]);
        assert_eq!(words, vec!["msh".to_owned(), "wrold".to_owned()]);
    }

    #[test]
    fn spellcheck_occurrence_index_counts_prior_matching_rows() {
        let misses = vec![
            SpellMiss {
                chars: 0..5,
                word: "wrold".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 8..12,
                word: "msh".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 16..21,
                word: "WROLD".to_owned(),
                suggestions: Vec::new(),
            },
        ];

        assert_eq!(spellcheck_occurrence_index(&misses, 0), 0);
        assert_eq!(spellcheck_occurrence_index(&misses, 1), 0);
        assert_eq!(spellcheck_occurrence_index(&misses, 2), 1);
        assert_eq!(spellcheck_occurrence_index(&misses, 99), 0);
    }

    #[test]
    fn browser_spellcheck_poll_retains_latest_page_text_result_and_marks_page() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let (tx, rx) = mpsc::channel();
        state.spellcheck.in_flight = Some(7);
        state.spellcheck.tab_index = Some(0);
        state.spellcheck.rx = Some(rx);
        tx.send((
            7,
            Ok(vec![SpellMiss {
                chars: 3..8,
                word: "wrold".to_owned(),
                suggestions: vec!["world".to_owned()],
            }]),
        ))
        .expect("send spell result");

        state.poll_spellcheck();

        let result = state
            .latest_spellcheck
            .as_ref()
            .expect("stored spellcheck result");
        assert_eq!(result.misses.len(), 1);
        assert_eq!(result.misses[0].word, "wrold");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: 1 possible misspelling; first: wrold -> world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetSpellcheckHighlights { words }
                    if words == &vec!["wrold".to_owned()]
            )),
            "spellcheck misses must be sent back to the helper as page highlights: {controls:?}"
        );
    }

    #[test]
    fn browser_spellcheck_correction_sends_selected_suggestion_to_result_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_spellcheck_correction(0, "wrold", "world");

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: replaced wrold with world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ApplySpellcheckCorrection {
                    word,
                    replacement,
                } if word == "wrold" && replacement == "world"
            )),
            "selected spelling suggestions must reach the helper: {controls:?}"
        );
    }

    #[test]
    fn browser_spellcheck_correction_all_sends_selected_suggestion_to_result_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_spellcheck_correction_all(0, "wrold", "world");

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: replaced all wrold with world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ApplySpellcheckCorrectionAll {
                    word,
                    replacement,
                } if word == "wrold" && replacement == "world"
            )),
            "all-occurrence spelling suggestions must reach the helper: {controls:?}"
        );
    }

    #[test]
    fn browser_spellcheck_correction_at_sends_selected_occurrence_to_result_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_spellcheck_correction_at(0, "wrold", "world", 2);

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: replaced occurrence 3 of wrold with world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ApplySpellcheckCorrectionAt {
                    word,
                    replacement,
                    occurrence,
                } if word == "wrold" && replacement == "world" && *occurrence == 2
            )),
            "indexed spelling suggestions must reach the helper: {controls:?}"
        );
    }

    #[test]
    fn cups_print_submission_uses_lp_title_and_pdf_path() {
        let dir = tempfile::tempdir().expect("cups pdf dir");
        let path = dir.path().join("page.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("pdf fixture");
        let mut seen_program = String::new();
        let mut seen_args = Vec::new();

        let job = submit_pdf_to_cups_with_runner(
            &path,
            "Magic Mesh Browser - Example",
            &CupsPrintSettings::default(),
            |program, args, timeout| {
                seen_program = program.to_owned();
                seen_args = args.to_vec();
                assert_eq!(timeout, CUPS_PRINT_TIMEOUT);
                Ok(ProcessOutput {
                    success: true,
                    stdout: "request id is Office-42 (1 file)\n".to_owned(),
                    stderr: String::new(),
                })
            },
        )
        .expect("lp submission succeeds");

        assert_eq!(job, "request id is Office-42 (1 file)");
        assert_eq!(seen_program, "lp");
        assert_eq!(
            seen_args,
            vec![
                "-t".to_owned(),
                "Magic Mesh Browser - Example".to_owned(),
                path.to_string_lossy().into_owned(),
            ]
        );
    }

    #[test]
    fn cups_print_submission_surfaces_lp_errors_without_a_printer() {
        let dir = tempfile::tempdir().expect("cups pdf dir");
        let path = dir.path().join("page.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("pdf fixture");

        let err = submit_pdf_to_cups_with_runner(
            &path,
            "Example",
            &CupsPrintSettings::default(),
            |_program, _args, _timeout| {
                Ok(ProcessOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "lp: Error - no default destination available\n".to_owned(),
                })
            },
        )
        .expect_err("lp failure is surfaced");

        assert_eq!(err, "lp: Error - no default destination available");
    }

    #[test]
    fn cups_print_submission_applies_destination_and_options() {
        let dir = tempfile::tempdir().expect("cups pdf dir");
        let path = dir.path().join("page.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("pdf fixture");
        let settings = CupsPrintSettings {
            destination: Some("Office".to_owned()),
            copies: 3,
            duplex: true,
            grayscale: true,
            ..Default::default()
        };
        let mut seen_args = Vec::new();

        submit_pdf_to_cups_with_runner(&path, "Example", &settings, |_program, args, _timeout| {
            seen_args = args.to_vec();
            Ok(ProcessOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
        .expect("lp submission succeeds");

        assert_eq!(
            seen_args,
            vec![
                "-d".to_owned(),
                "Office".to_owned(),
                "-n".to_owned(),
                "3".to_owned(),
                "-o".to_owned(),
                "sides=two-sided-long-edge".to_owned(),
                "-o".to_owned(),
                "ColorModel=Gray".to_owned(),
                "-t".to_owned(),
                "Example".to_owned(),
                path.to_string_lossy().into_owned(),
            ]
        );
    }

    #[test]
    fn cups_printer_discovery_marks_the_default_destination() {
        let printers = discover_cups_printers_with_runner(|program, args, timeout| {
            assert_eq!(program, "lpstat");
            assert_eq!(timeout, CUPS_PRINT_TIMEOUT);
            if args == ["-e"] {
                Ok(ProcessOutput {
                    success: true,
                    stdout: "Lab\nOffice\nLab\n".to_owned(),
                    stderr: String::new(),
                })
            } else if args == ["-d"] {
                Ok(ProcessOutput {
                    success: true,
                    stdout: "system default destination: Office\n".to_owned(),
                    stderr: String::new(),
                })
            } else {
                panic!("unexpected lpstat args: {args:?}");
            }
        })
        .expect("printer discovery succeeds");

        assert_eq!(
            printers,
            vec![
                CupsPrinter {
                    name: "Office".to_owned(),
                    is_default: true,
                },
                CupsPrinter {
                    name: "Lab".to_owned(),
                    is_default: false,
                },
            ]
        );
    }

    #[test]
    fn container_tabs_are_named_per_tab_and_visible_in_chrome() {
        let (first, _first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);

        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.active, 1);
        assert_eq!(state.tabs[0].container, ContainerProfile::None);
        assert_eq!(state.tabs[1].container, ContainerProfile::None);

        state.set_active_tab_container(ContainerProfile::Work);
        assert_eq!(state.tabs[1].container, ContainerProfile::Work);
        assert_eq!(
            state.tabs[0].container,
            ContainerProfile::None,
            "container identity stays per-tab"
        );
        assert!(
            tab_label(&state.tabs[1]).contains("W "),
            "the tab pill carries the Work marker"
        );
        assert!(
            tab_hover(&state.tabs[1]).contains("Container: Work"),
            "the hover text names the container"
        );

        state.cycle_active_tab_container();
        assert_eq!(state.tabs[1].container, ContainerProfile::Banking);
        state.select_tab(0);
        state.cycle_active_tab_container();
        assert_eq!(state.tabs[0].container, ContainerProfile::Personal);
        assert_eq!(state.tabs[1].container, ContainerProfile::Banking);
    }

    #[test]
    fn display_targets_are_per_tab_and_visible_in_chrome() {
        let (first, _first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);

        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.active, 1);
        assert_eq!(state.tabs[0].display_target, DisplayTarget::Current);
        assert_eq!(state.tabs[1].display_target, DisplayTarget::Current);

        state.set_active_tab_display_target(DisplayTarget::Secondary);
        assert_eq!(state.tabs[1].display_target, DisplayTarget::Secondary);
        assert_eq!(
            state.tabs[0].display_target,
            DisplayTarget::Current,
            "display target intent stays per-tab"
        );
        assert!(
            tab_label(&state.tabs[1]).contains("D2 "),
            "the tab pill carries the Display 2 marker"
        );
        assert!(
            tab_hover(&state.tabs[1]).contains("Display target: Secondary Display"),
            "the hover text names the display target"
        );

        state.cycle_active_tab_display_target();
        assert_eq!(state.tabs[1].display_target, DisplayTarget::AllDisplays);
        state.select_tab(0);
        state.cycle_active_tab_display_target();
        assert_eq!(state.tabs[0].display_target, DisplayTarget::Primary);
        assert_eq!(state.tabs[1].display_target, DisplayTarget::AllDisplays);
    }

    #[test]
    fn display_target_changes_publish_platform_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);

        state.set_active_tab_display_target(DisplayTarget::Secondary);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_DISPLAY_TARGET, None)
            .expect("list display target actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("handoff body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_display_target");
        assert_eq!(v["tab_index"], 0);
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["target"], "secondary");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");

        state.set_active_tab_display_target(DisplayTarget::AllDisplays);
        let msgs = persist
            .list_since(ACTION_BROWSER_DISPLAY_TARGET, None)
            .expect("list display target actions");
        assert_eq!(msgs.len(), 2);
        let body = msgs[1].body.as_deref().expect("second handoff body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["target"], "all_displays");
    }

    #[test]
    fn inactive_idle_tabs_publish_suspend_handoff_and_stop_once() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (first, first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(first, BrowserEngine::Servo);
        state.push_session_with_engine(second, BrowserEngine::Cef);
        assert_eq!(state.active, 1, "second tab is active");
        state.tabs[0].session.poll();
        state.tabs[1].session.poll();
        let _ = drain_control_messages(&first_helper);

        let now = Instant::now();
        state.tabs[0].last_activity = now - IDLE_TAB_SUSPEND_AFTER - Duration::from_secs(1);
        state.suspend_idle_tabs(now);
        state.suspend_idle_tabs(now + Duration::from_secs(1));

        assert!(state.tabs[0].idle_suspended);
        assert!(!state.tabs[1].idle_suspended, "active tab is not suspended");
        let controls = drain_control_messages(&first_helper);
        assert!(
            controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::Stop)),
            "inactive idle tab received a Stop control: {controls:?}"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_TAB_SUSPEND, None)
            .expect("list browser suspend actions");
        assert_eq!(msgs.len(), 1, "suspend handoff is once per idle period");
        let body = msgs[0].body.as_deref().expect("suspend body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_tab_suspend");
        assert_eq!(v["tab_index"], 0);
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["source"], "browser");
        assert_eq!(
            v["idle_after_ms"],
            u64::try_from(IDLE_TAB_SUSPEND_AFTER.as_millis()).unwrap()
        );
        assert!(
            tab_label(&state.tabs[0]).contains('\u{25D2}'),
            "suspended tabs wear the idle marker"
        );
        assert!(tab_hover(&state.tabs[0]).contains("Idle suspended"));
    }

    #[test]
    fn selecting_a_suspended_tab_reactivates_idle_state() {
        let (first, _first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.tabs[0].idle_suspended = true;
        let old_activity = state.tabs[0].last_activity;

        state.select_tab(0);

        assert_eq!(state.active, 0);
        assert!(!state.tabs[0].idle_suspended);
        assert!(state.tabs[0].last_activity >= old_activity);
    }

    #[test]
    fn no_session_paints_the_gated_empty_state() {
        let mut state = WebState::default();
        assert!(run_panel(&mut state), "the gated EmptyState drew nothing");
        assert!(state.tabs.is_empty());
    }

    #[test]
    fn a_paint_ready_frame_uploads_to_a_texture_and_paints() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            run_until_texture(&mut state),
            "no frame uploaded to a texture"
        );
        assert!(state.tabs[0].texture.is_some());
        assert!(run_panel(&mut state), "the browser image produced no draw");
    }

    #[test]
    fn tab_strip_state_switches_closes_and_requests_new_tabs() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        assert!(run_until_texture(&mut state));
        assert_eq!(state.active, 1, "new pushed tabs become foreground");

        state.select_tab(0);
        assert_eq!(state.active, 0);
        assert_eq!(state.address, "about:blank");

        state.close_tab(0);
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.active, 0, "active index stays valid after close");

        state.request_new_tab(BrowserEngine::Cef);
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForeground(BrowserEngine::Cef))
        );
        assert_eq!(state.take_open_request(), None, "request drains once");
    }

    #[test]
    fn tab_reorder_preserves_the_active_session_identity() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let (third, _helper3) = testkit::connect().expect("connect 3");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.push_session(third);
        assert!(run_until_texture(&mut state));
        state.select_tab(1);
        let active_title = state.tabs[state.active].session.title().to_owned();

        state.move_tab(1, 0);
        assert_eq!(state.active, 0);
        assert_eq!(state.tabs[state.active].session.title(), active_title);

        state.move_tab(0, 2);
        assert_eq!(state.active, 2);
        assert_eq!(state.tabs[state.active].session.title(), active_title);
    }

    /// A `WebState` with `n` (≤4) tabs, each tagged with a distinct container so a
    /// pinned recluster can be asserted by *identity* (tabs 0..n → Personal, Work,
    /// Banking, Research). No live helper — the pin/reorder methods never poll.
    fn tagged_tabs(n: usize) -> WebState {
        let mut state = WebState::default();
        for _ in 0..n {
            let (shell, _peer) = UnixStream::pair().expect("socketpair");
            state.push_session(WebSession::from_stream(shell, None).expect("session"));
        }
        for (i, tab) in state.tabs.iter_mut().enumerate() {
            tab.container = ContainerProfile::ALL[i + 1]; // skip None (index 0)
        }
        state
    }

    #[test]
    fn pinning_a_tab_clusters_it_to_the_front_preserving_order() {
        let mut state = tagged_tabs(3); // [Personal, Work, Banking]
        state.set_tab_pinned(2, true); // pin the Banking tab
                                       // The pinned tab jumps to the front; the unpinned tail keeps its order.
        assert!(state.tabs[0].pinned);
        assert!(!state.tabs[1].pinned && !state.tabs[2].pinned);
        assert_eq!(state.tabs[0].container, ContainerProfile::Banking);
        assert_eq!(state.tabs[1].container, ContainerProfile::Personal);
        assert_eq!(state.tabs[2].container, ContainerProfile::Work);
    }

    #[test]
    fn pinning_tracks_the_active_tab_across_the_recluster() {
        let mut state = tagged_tabs(3);
        state.select_tab(1); // active = the Work tab
        state.set_tab_pinned(2, true); // recluster → [Banking, Personal, Work]
        assert_eq!(state.active, 2);
        assert_eq!(state.tabs[state.active].container, ContainerProfile::Work);
    }

    #[test]
    fn unpinning_returns_a_tab_to_the_front_of_the_unpinned_cluster() {
        let mut state = tagged_tabs(3); // [Personal, Work, Banking]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned: [Personal, Work]; unpinned: [Banking]
        state.set_tab_pinned(0, false); // unpin Personal → rejoins unpinned front
        assert!(state.tabs[0].pinned);
        assert_eq!(state.tabs[0].container, ContainerProfile::Work); // still pinned
        assert!(!state.tabs[1].pinned);
        assert_eq!(state.tabs[1].container, ContainerProfile::Personal); // unpinned front
        assert_eq!(state.tabs[2].container, ContainerProfile::Banking);
    }

    #[test]
    fn a_drag_cannot_pull_an_unpinned_tab_ahead_of_a_pinned_one() {
        let mut state = tagged_tabs(3); // [Personal, Work, Banking]
        state.set_tab_pinned(0, true); // Personal pinned at the front
        state.move_tab(2, 0); // try to drag Banking to the very front
                              // The pinned Personal tab stays at the front — the drag snapped back.
        assert!(state.tabs[0].pinned);
        assert_eq!(state.tabs[0].container, ContainerProfile::Personal);
    }

    #[test]
    fn the_audio_glyph_reflects_playback_and_mute() {
        // Silent + unmuted → no glyph (the strip stays quiet).
        assert_eq!(audio_glyph_for(false, false), None);
        // Audibly playing → the speaker, hover offers to mute.
        assert_eq!(
            audio_glyph_for(true, false),
            Some(("\u{1F50A}", "Mute tab"))
        );
        // Muted → the muted-speaker; mute WINS the glyph even while audio plays.
        assert_eq!(
            audio_glyph_for(false, true),
            Some(("\u{1F507}", "Unmute tab"))
        );
        assert_eq!(
            audio_glyph_for(true, true),
            Some(("\u{1F507}", "Unmute tab"))
        );
    }

    #[test]
    fn an_audible_tab_renders_the_speaker_without_panic() {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::AudioState { audible: true }.encode(),
        ))
        .expect("audio event");
        state.tabs[0].session.poll();
        assert!(state.tabs[0].session.audible(), "the tab is now audible");

        // The strip must paint the speaker glyph without panicking, muted or not.
        let ctx = egui::Context::default();
        run_tab_strip_frame(&ctx, &mut state, body_input());
        state.tabs[0].muted = true;
        run_tab_strip_frame(&ctx, &mut state, body_input());
    }

    #[test]
    fn duplicating_a_tab_enqueues_an_open_on_the_same_url() {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://dup.example/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();

        state.duplicate_tab(0);
        assert!(
            matches!(
                state.open_requested.front(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://dup.example/"
            ),
            "duplicate enqueues a same-URL foreground open"
        );
    }

    #[test]
    fn close_other_tabs_keeps_only_the_target_and_pinned() {
        let mut state = tagged_tabs(4); // [Personal, Work, Banking, Research]
        state.set_tab_pinned(0, true); // Personal pinned, stays at the front
        let banking = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Banking)
            .expect("banking tab");
        state.close_other_tabs(banking);
        // Survivors: the pinned Personal tab + the kept Banking tab.
        assert_eq!(state.tabs.len(), 2);
        assert!(state
            .tabs
            .iter()
            .any(|t| t.container == ContainerProfile::Personal && t.pinned));
        assert_eq!(
            state.tabs[state.active].container,
            ContainerProfile::Banking
        );
    }

    #[test]
    fn close_tabs_to_the_right_closes_the_tail() {
        let mut state = tagged_tabs(4); // [Personal, Work, Banking, Research]
        state.close_tabs_to_the_right(1); // close Banking + Research
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.tabs[0].container, ContainerProfile::Personal);
        assert_eq!(state.tabs[1].container, ContainerProfile::Work);
    }

    #[test]
    fn close_tabs_to_the_right_spares_pinned_tabs() {
        let mut state = tagged_tabs(4);
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // Personal, Work pinned at the front
        state.close_tabs_to_the_right(0); // from the first pinned tab
        assert!(
            state
                .tabs
                .iter()
                .any(|t| t.container == ContainerProfile::Work && t.pinned),
            "the other pinned tab is spared"
        );
        assert!(
            !state
                .tabs
                .iter()
                .any(|t| t.container == ContainerProfile::Banking),
            "the unpinned tail is closed"
        );
    }

    #[test]
    fn tab_search_filters_on_title_and_url() {
        let mut state = WebState::default();
        let mut _peers = Vec::new();
        for url in [
            "https://news.example/",
            "https://mail.example/",
            "https://maps.example/",
        ] {
            let (shell, helper) = UnixStream::pair().expect("socketpair");
            helper.set_nonblocking(true).expect("helper nonblocking");
            state.push_session(WebSession::from_stream(shell, None).expect("session"));
            let idx = state.tabs.len() - 1;
            let mut peer: &UnixStream = &helper;
            peer.write_all(&wire::frame(
                &mde_web_preview_client::EventMsg::NavState {
                    can_back: false,
                    can_forward: false,
                    loading: false,
                    url: url.to_owned(),
                }
                .encode(),
            ))
            .expect("nav");
            state.tabs[idx].session.poll();
            _peers.push(helper); // keep the peers alive so the sessions don't crash
        }

        // Empty query → the full list.
        assert_eq!(matching_tab_indices(&state.tabs, ""), vec![0, 1, 2]);
        // A URL-substring match, case-insensitive.
        assert_eq!(matching_tab_indices(&state.tabs, "mail"), vec![1]);
        assert_eq!(matching_tab_indices(&state.tabs, "MAPS"), vec![2]);
        // No match → empty.
        assert!(matching_tab_indices(&state.tabs, "zzz").is_empty());
    }

    #[test]
    fn permission_grant_is_remembered_and_auto_allows_next_time() {
        assert_eq!(permission_kind_label(0), "know your location");
        assert_eq!(permission_kind_label(2), "access the clipboard");

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        let mut peer: &UnixStream = &helper;
        let request = mde_web_preview_client::EventMsg::PermissionRequest {
            id: 5,
            kind: 0,
            origin: "https://maps.example".to_owned(),
        };

        peer.write_all(&wire::frame(&request.encode()))
            .expect("req");
        state.tabs[0].session.poll();
        // First time: not granted → a prompt is offered (nothing auto-answered).
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://maps.example".to_owned(), 0))
        );

        // Allow it → remembered, and the pending request is answered + cleared.
        state.answer_active_permission("https://maps.example", 0, true);
        assert!(state.is_permission_granted("https://maps.example", 0));
        assert!(state.tabs[0].session.pending_permission().is_none());

        // A second identical request auto-allows with no prompt.
        peer.write_all(&wire::frame(&request.encode()))
            .expect("req2");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            None,
            "a granted capability auto-allows, no prompt"
        );
        assert!(
            state.tabs[0].session.pending_permission().is_none(),
            "the auto-allow answered and cleared the request"
        );
    }

    #[test]
    fn session_login_store_matches_by_host_updates_and_removes() {
        let mut state = WebState::default();
        state.save_login("Mail.Example.com", " alice ", "pw1"); // host lowercased, user trimmed
        state.save_login("mail.example.com", "bob", "pw2");
        state.save_login("other.example", "carol", "pw3");
        let m = state.logins_for_host("mail.example.com");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].username, "alice");

        // Re-saving same host+username UPDATES the password (no duplicate).
        state.save_login("mail.example.com", "alice", "pw1-new");
        let m = state.logins_for_host("mail.example.com");
        assert_eq!(m.len(), 2);
        assert_eq!(
            m.iter().find(|l| l.username == "alice").unwrap().password,
            "pw1-new"
        );

        // Blank host/username/password entries are ignored.
        state.save_login("mail.example.com", "", "x");
        state.save_login("", "u", "p");
        state.save_login("mail.example.com", "dave", "");
        assert_eq!(state.logins_for_host("mail.example.com").len(), 2);

        // Remove by index.
        let before = state.session_logins.len();
        state.remove_login(0);
        assert_eq!(state.session_logins.len(), before - 1);
        state.remove_login(999); // out of range is a no-op
        assert_eq!(state.session_logins.len(), before - 1);
    }

    #[test]
    fn filling_a_login_sends_the_credential_to_the_helper() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        state.fill_active_login("alice@example.com".to_owned(), "hunter2".to_owned());
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|m| matches!(
                m,
                mde_web_preview_client::ControlMsg::FillLogin { username, password }
                    if username == "alice@example.com" && password == "hunter2"
            )),
            "fill_active_login sends the chosen credential to the page: {controls:?}"
        );
    }

    #[test]
    fn auto_captured_login_offers_to_save_then_dedups() {
        let mut state = WebState::default();
        // A captured submit (engine-beaconed JSON) → a pending save offer.
        state.handle_login_capture(
            r#"{"origin":"https://mail.example.com","username":"alice","password":"pw"}"#,
        );
        let pending = state.pending_login_save.clone().expect("a save offer");
        assert_eq!(pending.host, "mail.example.com");
        assert_eq!(pending.username, "alice");

        // Accept → save + clear.
        state.save_login(&pending.host, &pending.username, &pending.password);
        state.pending_login_save = None;
        assert_eq!(state.logins_for_host("mail.example.com").len(), 1);

        // The SAME credential again does NOT re-offer (dedup).
        state.handle_login_capture(
            r#"{"origin":"https://mail.example.com","username":"alice","password":"pw"}"#,
        );
        assert!(
            state.pending_login_save.is_none(),
            "an already-saved credential does not re-prompt"
        );

        // A CHANGED password DOES re-offer (to update).
        state.handle_login_capture(
            r#"{"origin":"https://mail.example.com","username":"alice","password":"pw2"}"#,
        );
        assert!(
            state.pending_login_save.is_some(),
            "a changed password re-offers"
        );

        // Malformed / blank captures are ignored.
        state.pending_login_save = None;
        state.handle_login_capture("not json at all");
        state.handle_login_capture(r#"{"origin":"https://x","username":"","password":"p"}"#);
        assert!(state.pending_login_save.is_none());
    }

    // ----------------------------------------------------------------------
    // Adversarial stress tests (2026-07-13): try to BREAK the pinned-cluster,
    // active-session-tracking, close-set, duplicate, and permission
    // invariants. Each tab-op test asserts the ACTIVE session's *identity*
    // (its container tag) is unchanged and the pinned-first invariant holds.
    // ----------------------------------------------------------------------

    /// The pinned-first invariant: no unpinned tab may precede any pinned tab.
    fn assert_pinned_first(state: &WebState) {
        let boundary = state.tabs.iter().take_while(|t| t.pinned).count();
        assert!(
            state.tabs.iter().skip(boundary).all(|t| !t.pinned),
            "pinned tabs must all precede unpinned tabs, got {:?}",
            state
                .tabs
                .iter()
                .map(|t| (t.container, t.pinned))
                .collect::<Vec<_>>()
        );
    }

    /// The container tag of whatever session is currently active.
    fn active_container(state: &WebState) -> ContainerProfile {
        state.tabs[state.active].container
    }

    /// A `WebState` with one *live-peer* session plus the retained peer end (so
    /// the session never crash-detects). Frames written to the peer are visible
    /// after `state.tabs[0].session.poll()`.
    fn live_single_tab() -> (WebState, std::os::unix::net::UnixStream) {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        (state, helper)
    }

    /// Push a `PermissionRequest` event onto the helper wire.
    fn send_permission_request(peer: &UnixStream, id: u64, kind: u8, origin: &str) {
        let mut p: &UnixStream = peer;
        p.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::PermissionRequest {
                id,
                kind,
                origin: origin.to_owned(),
            }
            .encode(),
        ))
        .expect("permission request frame");
    }

    // --- Target 1: sort_pinned_stable / set_tab_pinned ---

    #[test]
    fn pinning_the_active_tab_keeps_it_active_at_the_front() {
        let mut state = tagged_tabs(4); // [Personal, Work, Banking, Research]
        state.select_tab(1); // active = Work
        assert_eq!(active_container(&state), ContainerProfile::Work);
        state.set_tab_pinned(1, true); // pin the ACTIVE (Work) tab
        assert_pinned_first(&state);
        assert!(state.tabs[0].pinned && state.tabs[0].container == ContainerProfile::Work);
        assert_eq!(
            active_container(&state),
            ContainerProfile::Work,
            "the active session is still Work after it reclustered to the front"
        );
    }

    #[test]
    fn pinning_last_then_first_preserves_active_identity() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.select_tab(2); // active = Banking
        state.set_tab_pinned(3, true); // pin Research (last) -> reclusters
        assert_pinned_first(&state);
        assert_eq!(active_container(&state), ContainerProfile::Banking);
        // Then pin whatever unpinned tab now leads the tail (Personal).
        let personal = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Personal)
            .expect("personal");
        state.set_tab_pinned(personal, true);
        assert_pinned_first(&state);
        assert_eq!(
            active_container(&state),
            ContainerProfile::Banking,
            "active stays Banking through two pins that reordered the strip",
        );
    }

    #[test]
    fn unpinning_the_middle_of_three_pinned_keeps_the_invariant() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true);
        state.set_tab_pinned(2, true); // pinned [P, W, B], unpinned [R]
        state.select_tab(3); // active = Research (unpinned)
        state.set_tab_pinned(1, false); // unpin the MIDDLE pinned tab (Work)
        assert_pinned_first(&state);
        // Work rejoins the FRONT of the unpinned cluster, ahead of Research.
        let work = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Work)
            .expect("work");
        let research = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Research)
            .expect("research");
        assert!(!state.tabs[work].pinned && work < research);
        assert_eq!(active_container(&state), ContainerProfile::Research);
    }

    #[test]
    fn pinning_all_in_place_then_unpinning_all_tracks_active() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.select_tab(1); // active = Work
        for i in 0..4 {
            state.set_tab_pinned(i, true);
        }
        assert!(state.tabs.iter().all(|t| t.pinned));
        // Pinning an already-front-clustered strip must NOT reorder it.
        assert_eq!(
            state.tabs.iter().map(|t| t.container).collect::<Vec<_>>(),
            vec![
                ContainerProfile::Personal,
                ContainerProfile::Work,
                ContainerProfile::Banking,
                ContainerProfile::Research,
            ],
        );
        assert_eq!(active_container(&state), ContainerProfile::Work);
        // Unpin every tab (drain the pinned cluster from the front).
        while let Some(i) = state.tabs.iter().position(|t| t.pinned) {
            state.set_tab_pinned(i, false);
        }
        assert!(state.tabs.iter().all(|t| !t.pinned));
        assert_eq!(
            active_container(&state),
            ContainerProfile::Work,
            "active stays Work across pin-all then unpin-all",
        );
    }

    // --- Target 2: move_tab with pinned tabs ---

    #[test]
    fn dragging_an_unpinned_tab_to_the_front_snaps_behind_the_pins() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned [P, W], unpinned [B, R]
        state.select_tab(3); // active = Research
        state.move_tab(3, 0); // drag Research to the very front — can't leap pins
        assert_pinned_first(&state);
        assert!(
            state.tabs[0].pinned && state.tabs[1].pinned,
            "both pins still lead the strip"
        );
        assert_eq!(
            active_container(&state),
            ContainerProfile::Research,
            "the dragged Research tab stays the active session",
        );
        assert_eq!(
            state.tabs[2].container,
            ContainerProfile::Research,
            "Research landed at the FRONT of the unpinned cluster",
        );
    }

    #[test]
    fn dragging_a_pinned_tab_to_the_end_snaps_back_to_the_front() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true); // Personal pinned at the front
        state.select_tab(0); // active = the pinned Personal tab
        state.move_tab(0, 3); // drag the pinned tab to the very end
        assert_pinned_first(&state);
        assert!(state.tabs[0].pinned && state.tabs[0].container == ContainerProfile::Personal);
        assert_eq!(active_container(&state), ContainerProfile::Personal);
    }

    #[test]
    fn moving_a_tab_across_the_active_index_preserves_the_active_session() {
        // Right-of-active to left-of-active (crosses active).
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.select_tab(1); // active = Work
        state.move_tab(3, 0); // Research jumps to the front, crossing Work
        assert_eq!(active_container(&state), ContainerProfile::Work);
        // The mirror: left-of-active to right-of-active.
        let mut state = tagged_tabs(4);
        state.select_tab(2); // active = Banking
        state.move_tab(0, 3); // Personal moves to the end, crossing Banking
        assert_eq!(active_container(&state), ContainerProfile::Banking);
    }

    // --- Target 3: close_other_tabs(keep) ---

    #[test]
    fn close_other_tabs_spares_pins_on_both_sides_of_the_kept_tab() {
        // Deliberately construct a NON-front-clustered pin layout (pins straddling
        // an unpinned tab) to stress the right-to-left index math directly.
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.tabs[0].pinned = true; // Personal pinned (before keep)
        state.tabs[2].pinned = true; // Banking pinned (after keep)
        state.close_other_tabs(1); // keep the unpinned Work tab at index 1
        assert_eq!(state.tabs.len(), 3);
        let survivors: Vec<_> = state.tabs.iter().map(|t| t.container).collect();
        assert!(survivors.contains(&ContainerProfile::Personal));
        assert!(survivors.contains(&ContainerProfile::Work));
        assert!(survivors.contains(&ContainerProfile::Banking));
        assert!(!survivors.contains(&ContainerProfile::Research));
        assert_eq!(
            active_container(&state),
            ContainerProfile::Work,
            "the explicitly-kept tab ends active even with pins on both sides",
        );
    }

    #[test]
    fn close_other_tabs_keeping_a_pinned_tab_survives_all_pins() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned [P, W], unpinned [B, R]
        let work = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Work)
            .expect("work");
        state.close_other_tabs(work); // keep a PINNED tab
        assert_eq!(state.tabs.len(), 2);
        assert!(state.tabs.iter().all(|t| t.pinned));
        assert_eq!(active_container(&state), ContainerProfile::Work);
    }

    #[test]
    fn close_other_tabs_keep_first_and_keep_last_leave_one_tab() {
        let mut state = tagged_tabs(4);
        state.close_other_tabs(0); // keep index 0 (Personal)
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(active_container(&state), ContainerProfile::Personal);

        let mut state = tagged_tabs(4);
        state.close_other_tabs(3); // keep the last (Research)
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(active_container(&state), ContainerProfile::Research);
    }

    // --- Target 4: close_tabs_to_the_right(from) ---

    #[test]
    fn close_tabs_to_the_right_spares_a_pin_in_the_middle() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.tabs[1].pinned = true; // Work pinned at index 1 (non-front layout)
        state.close_tabs_to_the_right(0);
        let survivors: Vec<_> = state.tabs.iter().map(|t| t.container).collect();
        assert!(survivors.contains(&ContainerProfile::Personal)); // index 0, untouched
        assert!(survivors.contains(&ContainerProfile::Work)); // pinned → spared
        assert!(!survivors.contains(&ContainerProfile::Banking));
        assert!(!survivors.contains(&ContainerProfile::Research));
    }

    #[test]
    fn close_tabs_to_the_right_from_boundary_and_noop_cases() {
        // From past the pinned boundary: only the unpinned tail to the right closes.
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned [P, W], unpinned [B, R]
        state.close_tabs_to_the_right(2); // from Banking (first unpinned)
        assert_eq!(state.tabs.len(), 3);
        assert!(!state
            .tabs
            .iter()
            .any(|t| t.container == ContainerProfile::Research));

        // from == last index is a no-op.
        let mut state = tagged_tabs(4);
        state.close_tabs_to_the_right(3);
        assert_eq!(state.tabs.len(), 4);

        // from past the end is a no-op (early return, no panic).
        let mut state = tagged_tabs(4);
        state.close_tabs_to_the_right(99);
        assert_eq!(state.tabs.len(), 4);
    }

    // --- Target 5: duplicate_tab ---

    #[test]
    fn duplicating_enqueues_at_the_back_of_the_open_queue() {
        let (mut state, peer) = live_single_tab();
        // A prior queued open, so we can prove duplicate lands at the BACK.
        let engine = state.engine;
        state.request_new_tab(engine);
        let mut p: &UnixStream = &peer;
        p.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://dup.example/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();
        state.duplicate_tab(0);
        assert!(
            matches!(
                state.open_requested.back(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://dup.example/"
            ),
            "duplicate lands a same-URL open at the BACK of the queue, got {:?}",
            state.open_requested.back(),
        );
    }

    #[test]
    fn duplicating_a_blank_tab_enqueues_a_blank_new_tab() {
        let (mut state, _peer) = live_single_tab();
        // A fresh session has no committed URL yet.
        assert!(state.tabs[0].session.nav().url.trim().is_empty());
        state.duplicate_tab(0);
        assert!(
            matches!(
                state.open_requested.back(),
                Some(TabOpenIntent::NewForeground(_))
            ),
            "a blank tab duplicates to a blank foreground tab, got {:?}",
            state.open_requested.back(),
        );
    }

    // --- Target 6: permission grant/answer/prompt ---

    #[test]
    fn a_grant_does_not_auto_allow_a_different_capability_kind() {
        let (mut state, peer) = live_single_tab();
        // Grant (origin A, kind 0 = geolocation).
        send_permission_request(&peer, 1, 0, "https://a.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://a.example".to_owned(), 0))
        );
        state.answer_active_permission("https://a.example", 0, true);
        assert!(state.is_permission_granted("https://a.example", 0));

        // Same origin, DIFFERENT kind (1 = notifications) must STILL prompt.
        send_permission_request(&peer, 2, 1, "https://a.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://a.example".to_owned(), 1)),
            "a geolocation grant must not silently allow notifications",
        );
        assert!(
            state.tabs[0].session.pending_permission().is_some(),
            "the different-kind request was NOT auto-answered",
        );
    }

    #[test]
    fn a_grant_does_not_auto_allow_a_different_origin() {
        let (mut state, peer) = live_single_tab();
        send_permission_request(&peer, 1, 0, "https://a.example");
        state.tabs[0].session.poll();
        state.answer_active_permission("https://a.example", 0, true);
        assert!(state.is_permission_granted("https://a.example", 0));

        // DIFFERENT origin, SAME kind must still prompt.
        send_permission_request(&peer, 2, 0, "https://b.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://b.example".to_owned(), 0)),
            "a grant to a.example must not silently allow b.example",
        );
    }

    #[test]
    fn a_blocked_capability_is_not_remembered_and_reprompts() {
        let (mut state, peer) = live_single_tab();
        send_permission_request(&peer, 1, 2, "https://c.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://c.example".to_owned(), 2))
        );
        // BLOCK it.
        state.answer_active_permission("https://c.example", 2, false);
        assert!(
            !state.is_permission_granted("https://c.example", 2),
            "a block is not a grant",
        );
        assert!(
            state.tabs[0].session.pending_permission().is_none(),
            "the block answered and cleared the request",
        );

        // The very same request must prompt AGAIN (blocks are not sticky).
        send_permission_request(&peer, 2, 2, "https://c.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://c.example".to_owned(), 2)),
            "a previously-blocked capability re-prompts",
        );
    }

    // --- Robustness: out-of-range tab ops must be safe no-ops ---

    #[test]
    fn out_of_range_tab_ops_are_no_ops() {
        let mut state = tagged_tabs(2); // [Personal, Work]
        let before: Vec<_> = state.tabs.iter().map(|t| t.container).collect();
        state.set_tab_pinned(9, true);
        state.move_tab(9, 0);
        state.move_tab(0, 9);
        state.move_tab(1, 1); // from == to
        state.close_tabs_to_the_right(9);
        state.close_other_tabs(9);
        state.duplicate_tab(9);
        assert_eq!(
            state.tabs.iter().map(|t| t.container).collect::<Vec<_>>(),
            before,
        );
        assert!(
            state.open_requested.is_empty(),
            "an out-of-range duplicate enqueues nothing",
        );
        assert!(state.tabs.iter().all(|t| !t.pinned));
    }

    /// Drive ONE headless frame of just the tab strip (isolating it from the full
    /// panel's polling), mirroring `middle_clicking_a_tab_pill_closes_that_tab`.
    fn run_tab_strip_frame(ctx: &egui::Context, state: &mut WebState, input: egui::RawInput) {
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| tab_strip(ui, state));
        });
    }

    /// The laid-out pill centres the strip stashed on its last frame, in tab order.
    fn tab_pill_centers(ctx: &egui::Context) -> Vec<egui::Pos2> {
        ctx.data(|d| d.get_temp::<Vec<Rect>>(tab_pill_rects_id()))
            .unwrap_or_default()
            .iter()
            .map(|r| r.center())
            .collect()
    }

    /// Press on `from`, drag past egui's click threshold to `to`, then release —
    /// a real pointer drag-reorder gesture routed through the tab strip.
    fn drag_pill(ctx: &egui::Context, state: &mut WebState, from: egui::Pos2, to: egui::Pos2) {
        let mut press = body_input();
        press.events = vec![
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ];
        run_tab_strip_frame(ctx, state, press);

        let mut moved = body_input();
        moved.events = vec![egui::Event::PointerMoved(to)];
        run_tab_strip_frame(ctx, state, moved);

        let mut release = body_input();
        release.events = vec![egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }];
        run_tab_strip_frame(ctx, state, release);
    }

    #[test]
    fn tab_drag_target_index_picks_the_nearest_slot_center() {
        let pills = vec![
            (0, Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 20.0))),
            (1, Rect::from_min_size(pos2(100.0, 0.0), vec2(100.0, 20.0))),
            (2, Rect::from_min_size(pos2(200.0, 0.0), vec2(100.0, 20.0))),
        ];
        // Horizontal centres sit at x = 50, 150, 250.
        assert_eq!(
            tab_drag_target_index(&pills, pos2(260.0, 10.0), TabAxis::Horizontal),
            Some(2)
        );
        assert_eq!(
            tab_drag_target_index(&pills, pos2(160.0, 10.0), TabAxis::Horizontal),
            Some(1)
        );
        assert_eq!(
            tab_drag_target_index(&pills, pos2(40.0, 10.0), TabAxis::Horizontal),
            Some(0)
        );
        // The vertical axis compares Y instead of X.
        let stacked = vec![
            (0, Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 20.0))),
            (1, Rect::from_min_size(pos2(0.0, 20.0), vec2(100.0, 20.0))),
        ];
        assert_eq!(
            tab_drag_target_index(&stacked, pos2(50.0, 38.0), TabAxis::Vertical),
            Some(1)
        );
        assert_eq!(
            tab_drag_target_index(&[], pos2(0.0, 0.0), TabAxis::Horizontal),
            None
        );
    }

    #[test]
    fn dragging_a_tab_pill_reorders_it_and_keeps_the_active_tab() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let (c, _hc) = testkit::connect().expect("connect c");
        let mut state = WebState::default();
        state.push_session(a);
        state.push_session(b);
        state.push_session(c);
        // Distinct per-tab markers so we can follow both the dragged tab and the
        // active session across the reorder (testkit titles are identical).
        state.tabs[0].force_dark = true; // the tab we drag
        state.tabs[2].reader_mode = true; // the active session
        state.select_tab(2);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        // Settle one frame so the strip publishes its pill rects.
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        assert_eq!(centers.len(), 3, "three pills laid out");

        // Drag tab 0 onto tab 1's slot.
        drag_pill(&ctx, &mut state, centers[0], centers[1]);

        // The dragged tab moved from slot 0 to slot 1 ...
        assert!(state.tabs[1].force_dark, "the dragged tab landed in slot 1");
        assert!(!state.tabs[0].force_dark);
        // ... and the SAME session stays active (a reorder below the active index
        // leaves the active index in place, still pointing at tab C).
        assert_eq!(
            state.active, 2,
            "reorder below the active index leaves it put"
        );
        assert!(
            state.tabs[state.active].reader_mode,
            "the active tab is still the same session after the reorder"
        );
    }

    #[test]
    fn dragging_a_tab_across_the_active_index_adjusts_active() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let (c, _hc) = testkit::connect().expect("connect c");
        let mut state = WebState::default();
        state.push_session(a);
        state.push_session(b);
        state.push_session(c);
        state.tabs[1].reader_mode = true; // follow the active session (B)
        state.select_tab(1);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        assert_eq!(centers.len(), 3);

        // Drag tab 0 (A) past the active tab to the last slot.
        drag_pill(&ctx, &mut state, centers[0], centers[2]);

        // A moved to the end; B slid one slot left but is STILL the active session.
        assert_eq!(
            state.active, 0,
            "active index follows its session across the reorder"
        );
        assert!(
            state.tabs[state.active].reader_mode,
            "the same session (B) stays active after crossing the active index"
        );
    }

    #[test]
    fn a_tiny_pointer_move_on_a_pill_activates_instead_of_reordering() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let mut state = WebState::default();
        state.push_session(a);
        state.push_session(b);
        state.tabs[0].force_dark = true; // marker proving the order is unchanged
        assert_eq!(state.active, 1, "pushing two tabs leaves the second active");

        let ctx = egui::Context::default();
        Style::install(&ctx);
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        let from = centers[0];
        // A jitter well under egui's 6pt click threshold — must read as a CLICK.
        let nudged = from + vec2(2.0, 0.0);

        let mut press = body_input();
        press.events = vec![
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ];
        run_tab_strip_frame(&ctx, &mut state, press);

        let mut release = body_input();
        release.events = vec![
            egui::Event::PointerMoved(nudged),
            egui::Event::PointerButton {
                pos: nudged,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        run_tab_strip_frame(&ctx, &mut state, release);

        assert_eq!(
            state.active, 0,
            "a sub-threshold move is a click, so tab 0 activates"
        );
        assert!(
            state.tabs[0].force_dark,
            "a click must NOT reorder — the order is untouched"
        );
        assert_eq!(state.tabs.len(), 2, "and nothing was closed");
    }

    #[test]
    fn dragging_a_tab_reorders_in_the_vertical_strip() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let (c, _hc) = testkit::connect().expect("connect c");
        let mut state = WebState::default();
        state.push_session(a);
        state.push_session(b);
        state.push_session(c);
        state.set_vertical_tabs(true);
        state.tabs[0].force_dark = true; // follow the dragged (and active) tab
        state.select_tab(0);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        assert_eq!(centers.len(), 3, "three stacked pills laid out");

        // Drag the top pill DOWN onto the bottom slot (matched along Y).
        drag_pill(&ctx, &mut state, centers[0], centers[2]);

        assert!(
            state.tabs[2].force_dark,
            "the vertical drag moved the pill to the bottom slot"
        );
        assert_eq!(
            state.active, 2,
            "the dragged tab was active and its index followed the move"
        );
    }

    #[test]
    fn horizontal_tab_pills_shrink_to_a_floor_then_scroll_instead_of_wrapping() {
        // A roomy strip with few tabs keeps full-width pills.
        assert_eq!(horizontal_tab_pill_width(1200.0, 2), CHROME_TAB_W);
        // A crowded strip shrinks pills to the floor (never below), so the strip
        // scrolls in ONE row instead of stacking onto a second row.
        assert_eq!(horizontal_tab_pill_width(1200.0, 40), CHROME_TAB_MIN_W);
        // The floor holds even in an absurdly narrow strip.
        assert!(horizontal_tab_pill_width(40.0, 40) >= CHROME_TAB_MIN_W);
        // More tabs never widen a pill.
        assert!(horizontal_tab_pill_width(1200.0, 20) <= horizontal_tab_pill_width(1200.0, 4));
    }

    #[test]
    fn many_tabs_stay_on_one_scrolling_row_and_the_active_tab_stays_reachable() {
        let mut state = WebState::default();
        let mut _helpers = Vec::new();
        for _ in 0..20 {
            let (s, h) = testkit::connect().expect("connect");
            state.push_session(s);
            _helpers.push(h);
        }
        assert_eq!(state.tabs.len(), 20);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        // Measure the vertical space the strip consumes: a single scrolling row is
        // ~one tab tall (plus a scrollbar), NOT the many rows the old wrap made.
        let mut used_h = 0.0f32;
        let _ = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let top = ui.next_widget_position().y;
                tab_strip(ui, &mut state);
                used_h = ui.next_widget_position().y - top;
            });
        });
        assert!(
            used_h < CHROME_TAB_H * 3.0,
            "20 tabs must stay on ONE scrolling row (strip height {used_h})"
        );

        // The far tab is still selectable — it scrolls into view and renders.
        state.select_tab(19);
        assert_eq!(state.active, 19);
        assert!(
            run_panel(&mut state),
            "the active far tab renders in the single scrolling row"
        );
    }

    #[test]
    fn closing_the_last_tab_returns_to_the_honest_empty_state() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.close_tab(0);
        assert!(state.tabs.is_empty());
        assert_eq!(state.active, 0);
        assert!(state.address.is_empty());
        assert!(run_panel(&mut state), "empty browser state draws honestly");
    }

    #[test]
    fn tab_strip_renders_with_multiple_tabs() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        assert!(run_panel(&mut state), "tab strip produced no primitives");
    }

    #[test]
    fn vertical_tab_strip_renders_with_the_same_sessions() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.set_vertical_tabs(true);
        assert!(
            run_panel(&mut state),
            "vertical tabs produced no primitives"
        );
        assert_eq!(state.tabs.len(), 2);
        assert!(state.vertical_tabs);
    }

    #[test]
    fn a_well_formed_favicon_png_decodes_to_a_texture() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::RED;
        img.pixels[3] = egui::Color32::BLUE;
        let png = encode_color_image_png(&img).expect("encode a tiny favicon PNG");

        let decoded =
            crate::chooser::decode_png_rgba(&png).expect("a well-formed favicon PNG decodes");
        assert_eq!(decoded.size, [2, 2]);
    }

    #[test]
    fn garbage_favicon_bytes_fail_soft_instead_of_panicking() {
        assert!(
            crate::chooser::decode_png_rgba(b"not a png").is_none(),
            "malformed bytes decode to None, never a panic"
        );

        let mut state = WebState::default();
        state.push_session(session_with_favicon(b"not a png"));

        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(
            tab_favicon_texture(&ctx, &mut state.tabs[0]).is_none(),
            "a garbage favicon resolves to no texture, falling back to the pill's own glyph"
        );
        // The failed decode is still cached (fingerprint recorded, texture None) so
        // the same garbage bytes aren't re-decoded every frame.
        let cache = state.tabs[0]
            .favicon_cache
            .as_ref()
            .expect("a decode attempt is cached even on failure");
        assert!(cache.texture.is_none());
    }

    #[test]
    fn unchanged_favicon_bytes_reuse_the_cached_texture_and_changed_bytes_invalidate_it() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::RED;
        let png_a = encode_color_image_png(&img).expect("encode favicon A");
        img.pixels[0] = egui::Color32::BLUE;
        let png_b = encode_color_image_png(&img).expect("encode favicon B");

        let (mut session, peer) = raw_session_pair();
        send_favicon(&peer, &png_a);
        session.poll();

        let mut state = WebState::default();
        state.push_session(session);

        let ctx = egui::Context::default();
        Style::install(&ctx);

        let first = tab_favicon_texture(&ctx, &mut state.tabs[0]).expect("favicon A decodes");
        let second =
            tab_favicon_texture(&ctx, &mut state.tabs[0]).expect("favicon A decodes again");
        assert_eq!(
            first.id(),
            second.id(),
            "the same favicon bytes must reuse the cached texture, not re-decode"
        );

        // A genuinely new favicon (different bytes) invalidates the cache and gets
        // its own fresh texture — proving the fingerprint gates on content, not a
        // permanent "decoded once, ever" latch.
        send_favicon(&peer, &png_b);
        state.tabs[0].session.poll();
        let third = tab_favicon_texture(&ctx, &mut state.tabs[0]).expect("favicon B decodes");
        assert_ne!(
            second.id(),
            third.id(),
            "changed favicon bytes must decode a fresh texture"
        );
    }

    #[test]
    fn horizontal_tab_strip_renders_a_favicon_without_panicking() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::GREEN;
        let png = encode_color_image_png(&img).expect("encode favicon");

        let mut state = WebState::default();
        state.push_session(session_with_favicon(&png));
        assert!(
            run_panel(&mut state),
            "the horizontal tab strip with a favicon produced no primitives"
        );
        assert!(
            state.tabs[0]
                .favicon_cache
                .as_ref()
                .is_some_and(|cache| cache.texture.is_some()),
            "the frame should have decoded + cached the favicon texture"
        );
    }

    #[test]
    fn vertical_tab_strip_renders_a_favicon_without_panicking() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::GREEN;
        let png = encode_color_image_png(&img).expect("encode favicon");

        let mut state = WebState::default();
        state.push_session(session_with_favicon(&png));
        state.set_vertical_tabs(true);
        assert!(
            run_panel(&mut state),
            "the vertical tab strip with a favicon produced no primitives"
        );
        assert!(
            state.tabs[0]
                .favicon_cache
                .as_ref()
                .is_some_and(|cache| cache.texture.is_some()),
            "the frame should have decoded + cached the favicon texture"
        );
    }

    #[test]
    fn ctrl_tab_cycles_tabs_and_ctrl_digits_jump_to_them() {
        let (first, _h1) = testkit::connect().expect("connect 1");
        let (second, _h2) = testkit::connect().expect("connect 2");
        let (third, _h3) = testkit::connect().expect("connect 3");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.push_session(third);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert_eq!(state.active, 2, "the newest pushed tab starts foreground");

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, false)
        ));
        assert_eq!(state.active, 0, "Ctrl+Tab wraps forward to the first tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, false)
        ));
        assert_eq!(state.active, 1, "Ctrl+Tab advances to the next tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, true)
        ));
        assert_eq!(state.active, 0, "Ctrl+Shift+Tab cycles backwards");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, true)
        ));
        assert_eq!(state.active, 2, "Ctrl+Shift+Tab wraps back to the last tab");

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num1, false)
        ));
        assert_eq!(state.active, 0, "Ctrl+1 activates the first tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num3, false)
        ));
        assert_eq!(state.active, 2, "Ctrl+3 activates the third tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num1, false)
        ));
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num9, false)
        ));
        assert_eq!(state.active, 2, "Ctrl+9 activates the LAST tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num5, false)
        ));
        assert_eq!(state.active, 2, "an out-of-range Ctrl+digit is ignored");
    }

    #[test]
    fn ctrl_t_opens_a_new_tab_intent_and_never_leaks_into_the_page() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let engine = state.engine;
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        // Focus the page canvas first — the browser-reserved shortcut must
        // still win over page keyboard forwarding.
        let page_point = pos2(480.0, 420.0);
        let mut click = body_input();
        click.events = vec![
            egui::Event::PointerMoved(page_point),
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, click));
        assert!(
            state.tabs[0].page_focused,
            "the click latches page keyboard focus"
        );
        let _ = drain_control_messages(&helper);

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, false)
        ));
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForeground(engine)),
            "Ctrl+T raises the tab strip's exact new-tab intent"
        );
        let leaked = drain_control_messages(&helper);
        assert!(
            !leaked.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Key { .. }
                )
            )),
            "a consumed browser shortcut must not be forwarded to the page: {leaked:?}"
        );
    }

    #[test]
    fn ctrl_w_closes_and_ctrl_shift_t_restores_the_closed_page() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let engine = state.tabs[0].engine;
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::W, false)
        ));
        assert!(state.tabs.is_empty(), "Ctrl+W closes the active tab");
        assert_eq!(
            state.closed_tabs.last(),
            Some(&ClosedTab {
                url: "https://example.test/".to_owned(),
                title: "Example".to_owned(),
                engine,
            }),
            "the closed page is retained in-memory for reopen"
        );

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, true)
        ));
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine,
                url: "https://example.test/".to_owned(),
            }),
            "Ctrl+Shift+T reopens the closed page on its original engine"
        );
        // The stack drains — a second restore is an honest no-op.
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, true)
        ));
        assert_eq!(state.take_open_request(), None, "the reopen stack drains");
    }

    #[test]
    fn the_reopen_stack_is_bounded_and_skips_blank_sessions() {
        // A session that never committed a URL leaves nothing to restore.
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("nonblocking helper");
        let blank = WebSession::from_stream(shell, None).expect("session");
        let mut state = WebState::default();
        state.push_session(blank);
        state.close_tab(0);
        assert!(
            state.closed_tabs.is_empty(),
            "a blank session is not reopenable"
        );

        // The stack stays bounded: a close past the cap evicts the OLDEST.
        let (session, _helper2) = testkit::connect().expect("connect");
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.closed_tabs = (0..CLOSED_TAB_STACK_CAP)
            .map(|n| ClosedTab {
                url: format!("https://mesh{n}.test/"),
                title: format!("Mesh {n}"),
                engine: BrowserEngine::Servo,
            })
            .collect();
        state.close_tab(0);
        assert_eq!(
            state.closed_tabs.len(),
            CLOSED_TAB_STACK_CAP,
            "the reopen stack stays bounded"
        );
        assert_eq!(
            state.closed_tabs.first().map(|c| c.url.as_str()),
            Some("https://mesh1.test/"),
            "the oldest retained entry is evicted first"
        );
        assert_eq!(
            state.closed_tabs.last().map(|c| c.url.as_str()),
            Some("about:blank"),
            "the newest close is retained"
        );
    }

    #[test]
    fn middle_clicking_a_tab_pill_closes_that_tab() {
        use std::cell::Cell;
        let (first, _h1) = testkit::connect().expect("connect 1");
        let (second, _h2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        // Mark the FIRST tab so the assertion can tell which one closed.
        state.tabs[0].force_dark = true;
        let ctx = egui::Context::default();
        Style::install(&ctx);

        // Probe frame: record where the first tab pill lands.
        let origin = Cell::new(pos2(0.0, 0.0));
        let _ = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                origin.set(ui.next_widget_position());
                tab_strip(ui, &mut state);
            });
        });
        let point = origin.get() + vec2(CHROME_TAB_W * 0.5, CHROME_TAB_H * 0.5);

        let mut input = body_input();
        input.events = vec![
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Middle,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Middle,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| tab_strip(ui, &mut state));
        });
        assert_eq!(state.tabs.len(), 1, "middle-click closes the pill's tab");
        assert!(
            !state.tabs[0].force_dark,
            "the SECOND tab survives — middle-click closed the first pill"
        );
    }

    #[test]
    fn engine_navigation_updates_the_address_bar_only_when_not_editing() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "https://example.test/",
            "the pumped engine URL lands in the address bar with no chrome action"
        );

        // An engine-driven navigation (redirect / page script) rewrites the
        // bar on the next pump — the seam tab select/close/move never covered.
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://example.test/redirected".to_owned(),
            },
        );
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(state.address, "https://example.test/redirected");

        // Focus the omnibox and start a draft: an engine navigation must NOT
        // clobber the in-progress edit.
        ctx.memory_mut(|m| m.request_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        state.address = "mesh draft".to_owned();
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://example.test/second".to_owned(),
            },
        );
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "mesh draft",
            "an engine navigation never overwrites an in-progress edit"
        );

        // Blur without submitting: the missed engine URL is NOT retroactively
        // applied over the draft, but the NEXT engine navigation syncs again.
        ctx.memory_mut(|m| m.surrender_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "mesh draft",
            "blurring must not retroactively apply a stale engine URL"
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://example.test/third".to_owned(),
            },
        );
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "https://example.test/third",
            "engine navigation resumes syncing once the edit ends"
        );
    }

    #[test]
    fn open_close_shortcuts_pause_while_a_chrome_text_field_is_editing() {
        let (first, _h1) = testkit::connect().expect("connect 1");
        let (second, _h2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        ctx.memory_mut(|m| m.request_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert!(state.chrome_edit_focus, "omnibox focus latches the guard");

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::W, false)
        ));
        assert_eq!(state.tabs.len(), 2, "Ctrl+W must not close a tab mid-edit");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, false)
        ));
        assert_eq!(
            state.take_open_request(),
            None,
            "Ctrl+T must not open a tab mid-edit"
        );
        // Tab CYCLING stays live during edits (the desktop-browser idiom).
        let before = state.active;
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, false)
        ));
        assert_ne!(
            state.active, before,
            "Ctrl+Tab keeps cycling while the omnibox is focused"
        );

        // Blur → the open/close accelerators resume.
        ctx.memory_mut(|m| m.surrender_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert!(!state.chrome_edit_focus, "blurring releases the guard");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::W, false)
        ));
        assert_eq!(
            state.tabs.len(),
            1,
            "Ctrl+W closes the tab once the edit ends"
        );
    }

    #[test]
    fn new_tab_dashboard_renders_for_about_blank() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        assert_eq!(state.tabs[0].session.nav().url, "about:blank");
        assert!(run_panel(&mut state), "new-tab dashboard draws");
    }

    #[test]
    fn new_tab_dashboard_search_loads_mesh_searxng() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.dashboard_query = "mesh browser".to_owned();

        state.submit_dashboard_search();

        assert_eq!(state.address, "https://search.mesh/search?q=mesh+browser");
        assert_eq!(state.insecure_prompt, None);
        assert!(
            wait_for_fresh_frame(&mut state),
            "dashboard search reached the helper"
        );
    }

    #[test]
    fn new_tab_dashboard_mesh_service_shortcuts_use_the_same_load_gate() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        let music = NEW_TAB_SERVICES
            .iter()
            .find(|svc| svc.label == "Music")
            .expect("music shortcut");

        state.open_mesh_service(music.url.to_owned());

        assert_eq!(state.address, "http://music.mesh:4533/");
        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://music.mesh:4533/")
        );
        assert!(
            !state.tabs[0].session.nav().loading,
            "HTTP mesh services pause at the same HTTPS prompt"
        );
    }

    #[test]
    fn explicit_http_address_prompts_before_loading() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.address = "http://plain.example/path".to_owned();

        state.submit_address();

        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://plain.example/path")
        );
        assert_eq!(state.address, "http://plain.example/path");
        assert!(
            !state.tabs[0].session.nav().loading,
            "HTTP prompt pauses before sending Load to the helper"
        );
        assert!(run_panel(&mut state), "the HTTP prompt renders");
    }

    #[test]
    fn http_prompt_can_upgrade_or_continue() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.address = "http://plain.example/path".to_owned();
        state.submit_address();
        state.upgrade_insecure_load();
        assert_eq!(state.insecure_prompt, None);
        assert_eq!(state.address, "https://plain.example/path");
        assert!(
            wait_for_fresh_frame(&mut state),
            "upgraded HTTPS load reached the helper"
        );

        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.address = "http://plain.example/keep-http".to_owned();
        state.submit_address();
        state.continue_insecure_load();
        assert_eq!(state.insecure_prompt, None);
        assert_eq!(state.address, "http://plain.example/keep-http");
        assert!(
            wait_for_fresh_frame(&mut state),
            "continued HTTP load reached the helper"
        );
    }

    #[test]
    fn session_hsts_auto_upgrades_a_host_the_user_previously_upgraded() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        // First plain-http visit prompts; the user upgrades → the host is remembered.
        state.address = "http://shop.example/".to_owned();
        state.submit_address();
        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://shop.example/")
        );
        state.upgrade_insecure_load();
        assert!(state.hsts_hosts.contains("shop.example"));

        // A later plain-http nav to the SAME host auto-upgrades silently (no prompt).
        state.address = "http://shop.example/cart".to_owned();
        state.submit_address();
        assert!(
            state.insecure_prompt.is_none(),
            "a remembered host auto-upgrades without re-prompting"
        );

        // A different plain-http host still prompts.
        state.address = "http://other.example/".to_owned();
        state.submit_address();
        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://other.example/")
        );
    }

    #[test]
    fn fullscreen_mode_renders_the_body_only_and_toggles_back() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        // Immersive mode renders the page body without the chrome (no panic).
        state.fullscreen = true;
        assert!(
            run_panel(&mut state),
            "fullscreen renders the immersive body view"
        );
        // Exiting restores the full chrome.
        state.fullscreen = false;
        assert!(
            run_panel(&mut state),
            "exiting fullscreen restores the chrome"
        );
    }

    #[test]
    fn a_crashed_tab_paints_the_honest_crashed_state() {
        let (session, helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);

        helper.crash();
        // Poll it to observe the crash, then render the crashed body.
        assert!(run_panel(&mut state));
        assert!(state.tabs[0].session.is_crashed());
        assert!(run_panel(&mut state), "the crashed body produced no draw");
    }

    #[test]
    fn one_tabs_crash_does_not_disturb_another() {
        let (a, helper_a) = testkit::connect().expect("connect a");
        let (b, _helper_b) = testkit::connect().expect("connect b");
        let mut state = WebState::default();
        state.push_session(a); // tab 0
        state.push_session(b); // tab 1 (active)
        run_until_texture(&mut state);

        helper_a.crash();
        run_panel(&mut state); // polls all tabs
        assert!(state.tabs[0].session.is_crashed(), "tab 0 crashed");
        assert!(!state.tabs[1].session.is_crashed(), "tab 1 unaffected");
    }

    #[test]
    fn shows_cert_interstitial_reflects_presence_and_crash_precedence() {
        let err = CertError {
            url: "https://bad.example.com/x".to_owned(),
            code: -202,
            message: "The certificate authority is not trusted".to_owned(),
        };
        assert!(
            shows_cert_interstitial(false, Some(&err)),
            "a cert error on a live tab shows the interstitial"
        );
        assert!(
            !shows_cert_interstitial(false, None),
            "no cert error renders the normal body"
        );
        assert!(
            !shows_cert_interstitial(true, Some(&err)),
            "a crash always wins over a cert error"
        );
    }

    #[test]
    fn cert_error_host_extracts_the_domain() {
        let err = CertError {
            url: "https://bad.example.com/x".to_owned(),
            code: -202,
            message: "The certificate authority is not trusted".to_owned(),
        };
        assert_eq!(cert_error_host(&err), "bad.example.com");
    }

    #[test]
    fn cert_error_host_falls_back_to_the_raw_url_with_no_authority() {
        let err = CertError {
            url: "not-a-url".to_owned(),
            code: -202,
            message: "x".to_owned(),
        };
        assert_eq!(cert_error_host(&err), "not-a-url");
    }

    #[test]
    fn a_cert_error_paints_the_interstitial_instead_of_a_panic() {
        let session = session_with_cert_error(
            "https://bad.example.com/",
            -202,
            "The certificate authority is not trusted",
        );
        assert_eq!(
            session.cert_error(),
            Some(&CertError {
                url: "https://bad.example.com/".to_owned(),
                code: -202,
                message: "The certificate authority is not trusted".to_owned(),
            })
        );
        let mut state = WebState::default();
        state.push_session(session);

        // A render pass must not panic with a cert error present on the active
        // (and only) tab — it paints the interstitial in place of the frame.
        assert!(run_panel(&mut state), "the interstitial produced no draw");
    }

    #[test]
    fn a_safe_browsing_block_paints_the_interstitial_instead_of_a_panic() {
        use mde_web_preview_client::{EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        // The active tab is sitting on a benign page (with back-history)...
        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://start.example/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();

        // ...then a top-level navigation to a mesh-flagged unsafe host is blocked,
        // arming the full-page interstitial (a Document block, not a subresource).
        state.set_safe_browsing_hosts(["malware.test"]);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 7,
                url: "https://malware.test/".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Document),
            }
            .encode(),
        ))
        .expect("document request");
        state.tabs[0].session.poll();
        assert_eq!(
            state.tabs[0].session.safe_browsing_block(),
            Some("https://malware.test/"),
            "a top-level Document block arms the interstitial"
        );

        // The render pass paints the "unsafe site blocked" interstitial in place of
        // the frame and must not panic with the block present on the active tab.
        assert!(run_panel(&mut state), "the interstitial produced no draw");
    }

    #[test]
    fn a_permission_prompt_is_suppressed_behind_a_cert_interstitial() {
        // Defensive precedence: a tab that has BOTH a blocking cert error and a
        // pending permission must show the cert interstitial with the permission bar
        // suppressed — never paint a prompt over an interstitial, and never let the
        // combination panic. (A cert-blocked page can't really raise a request; this
        // guards the state anyway.)
        let (mut session, peer) = raw_session_pair();
        send_cert_error(&peer, "https://x.example/", -202, "bad cert");
        write_helper_event(
            &peer,
            &mde_web_preview_client::EventMsg::PermissionRequest {
                id: 3,
                kind: 0,
                origin: "https://x.example".to_owned(),
            },
        );
        session.poll();
        assert!(session.cert_error().is_some());
        assert!(session.pending_permission().is_some());

        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            run_panel(&mut state),
            "the cert interstitial produced a draw"
        );
        assert!(
            state.tabs[0].session.pending_permission().is_some(),
            "the prompt is held behind the interstitial, not consumed by a stray bar"
        );
    }

    #[test]
    fn the_active_tabs_cert_error_does_not_disturb_another_tab() {
        let clean = session_with_favicon(&[0x89, b'P', b'N', b'G']);
        let blocked = session_with_cert_error("https://bad.example.com/", -202, "not trusted");
        let mut state = WebState::default();
        state.push_session(clean); // tab 0
        state.push_session(blocked); // tab 1 (active)

        assert!(run_panel(&mut state), "the interstitial produced no draw");
        assert!(
            state.tabs[0].session.cert_error().is_none(),
            "tab 0 unaffected"
        );
        assert!(
            state.tabs[1].session.cert_error().is_some(),
            "tab 1 blocked"
        );
    }

    #[test]
    fn cert_error_back_action_prefers_history_over_closing() {
        assert!(
            matches!(cert_error_back_action(true), CertErrorBackAction::GoBack),
            "with back history, \"Back to safety\" navigates back"
        );
        assert!(
            matches!(cert_error_back_action(false), CertErrorBackAction::CloseTab),
            "with no back history, \"Back to safety\" closes the tab"
        );
    }

    #[test]
    fn back_to_safety_with_no_history_closes_the_tab() {
        let session = session_with_cert_error("https://bad.example.com/", -202, "not trusted");
        assert!(
            !session.nav().can_back,
            "a raw socketpair session starts with no back history"
        );
        let mut state = WebState::default();
        state.push_session(session);
        assert_eq!(state.tabs.len(), 1);
        assert!(run_panel(&mut state), "the interstitial produced no draw");

        // No pointer harness clicks the real button here (that needs the live
        // widget rect); this proves the wiring `active_body` takes on a click —
        // the pure `cert_error_back_action` decision — matches the tab's actual
        // history state.
        let can_back = state.tabs[0].session.nav().can_back;
        match cert_error_back_action(can_back) {
            CertErrorBackAction::GoBack => panic!("expected CloseTab with no history"),
            CertErrorBackAction::CloseTab => state.close_tab(0),
        }
        assert!(state.tabs.is_empty(), "the tab closed");
    }

    #[test]
    fn the_ad_filter_blocked_count_surfaces_on_the_active_tab() {
        use mde_web_preview_client::{
            wire, EventMsg, FilterListStore, RequestFilter, ResourceType, WebSession,
        };
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;

        // A bundled-filter session over a bare socketpair (no shm needed — we only
        // drive the request-policy protocol to bump the per-page counter).
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let filter = RequestFilter::from_store(&FilterListStore::with_bundled());
        let mut session = WebSession::from_stream(shell, None)
            .expect("session")
            .with_filter(filter);

        let mut peer: &UnixStream = &helper;
        let nav = EventMsg::NavState {
            can_back: false,
            can_forward: false,
            loading: false,
            url: "https://news.example.com/".to_owned(),
        };
        peer.write_all(&wire::frame(&nav.encode())).expect("nav");
        let req = EventMsg::ResourceRequest {
            id: 1,
            url: "https://doubleclick.net/ad".to_owned(),
            resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
        };
        peer.write_all(&wire::frame(&req.encode())).expect("req");
        session.poll();
        assert_eq!(
            session.blocked_count(),
            1,
            "the tracker was blocked + counted"
        );

        let mut state = WebState::default();
        state.tabs.push(Tab {
            session,
            engine: BrowserEngine::Servo,
            container: ContainerProfile::None,
            display_target: DisplayTarget::Current,
            group: None,
            pinned: false,
            muted: false,
            force_dark: false,
            reader_mode: false,
            user_scripts: false,
            user_agent: UserAgentOverride::Default,
            device_profile: DeviceProfile::Default,
            last_activity: Instant::now(),
            idle_suspended: false,
            page_focused: false,
            texture: None,
            last_frame: None,
            resizer: ViewportResizer::default(),
            favicon_cache: None,
        });
        // The nav chrome (with the "N blocked" shield) renders without panicking.
        assert!(run_panel(&mut state), "the browser chrome produced no draw");
        assert_eq!(state.tabs[0].session.blocked_count(), 1);
    }

    #[test]
    fn per_site_privacy_toggle_changes_real_resource_verdicts() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 1,
                url: "https://doubleclick.net/ad".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
            }
            .encode(),
        ))
        .expect("blocked request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 1,
                        allow: false
                    }
                )),
            "the bundled blocker rejects the tracker before network"
        );

        state.set_active_site_blocking(false);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 2,
                url: "https://doubleclick.net/ad".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
            }
            .encode(),
        ))
        .expect("allowlisted request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(m, ControlMsg::ResourceVerdict { id: 2, allow: true })),
            "allowlisting the current site changes the actual helper verdict"
        );

        state.set_active_site_blocking(true);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 3,
                url: "https://doubleclick.net/ad".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
            }
            .encode(),
        ))
        .expect("reblocked request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 3,
                        allow: false
                    }
                )),
            "re-enabling site blocking restores the block verdict"
        );
    }

    #[test]
    fn parse_safe_browsing_hosts_skips_comments_blanks_and_lowercases() {
        let text = "# operator blocklist\nMalware.test\n\n  Phish.example  \n# note\nads.bad\n";
        assert_eq!(
            parse_safe_browsing_hosts(text),
            vec![
                "malware.test".to_string(),
                "phish.example".to_string(),
                "ads.bad".to_string(),
            ]
        );
        assert!(parse_safe_browsing_hosts("# only comments\n\n   \n").is_empty());
    }

    #[test]
    fn safe_browsing_hosts_change_real_resource_verdicts() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();
        let _ = drain_control_messages(&helper);

        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 11,
                url: "https://malware.test/payload.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            }
            .encode(),
        ))
        .expect("unlisted request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 11,
                        allow: true
                    }
                )),
            "an unlisted host is allowed"
        );

        state.set_safe_browsing_hosts(["malware.test"]);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 12,
                url: "https://cdn.malware.test/payload.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            }
            .encode(),
        ))
        .expect("listed request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 12,
                        allow: false
                    }
                )),
            "loading the mesh-hosted safe-browsing host blocks the real helper request"
        );
    }

    #[test]
    fn site_data_manager_tracks_committed_first_party_hosts() {
        use mde_web_preview_client::EventMsg;

        let (shell_a, helper_a) = UnixStream::pair().expect("socketpair a");
        let (shell_b, helper_b) = UnixStream::pair().expect("socketpair b");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell_a, None).expect("session a"));
        state.push_session(WebSession::from_stream(shell_b, None).expect("session b"));

        let mut peer_a: &UnixStream = &helper_a;
        peer_a
            .write_all(&wire::frame(
                &EventMsg::NavState {
                    can_back: false,
                    can_forward: false,
                    loading: false,
                    url: "https://alpha.example/path".to_owned(),
                }
                .encode(),
            ))
            .expect("nav a");
        let mut peer_b: &UnixStream = &helper_b;
        peer_b
            .write_all(&wire::frame(
                &EventMsg::NavState {
                    can_back: false,
                    can_forward: false,
                    loading: false,
                    url: "https://beta.example/path".to_owned(),
                }
                .encode(),
            ))
            .expect("nav b");

        state.tabs[0].session.poll();
        state.tabs[1].session.poll();
        state.update_site_data_from_tabs();

        let summary = state.site_data_summary();
        assert!(summary.contains("2 tracked sites"), "summary = {summary}");
        assert!(summary.contains("2 open tabs"), "summary = {summary}");
    }

    #[test]
    fn clearing_current_tab_records_the_active_site_data_reset() {
        use mde_web_preview_client::EventMsg;

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/path".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();
        state.update_site_data_from_tabs();

        state.clear_active_session_data();
        let summary = state.site_data_summary();
        assert!(
            summary.contains("news.example.com cleared 1 time"),
            "summary = {summary}"
        );
    }

    #[test]
    fn custom_filter_rules_compile_into_open_tabs() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        state.add_custom_filter_rules("TestCustom", "||ads.custom.test^");

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://publisher.test/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 41,
                url: "https://ads.custom.test/banner.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            }
            .encode(),
        ))
        .expect("custom request");
        state.tabs[0].session.poll();

        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 41,
                        allow: false
                    }
                )),
            "custom EasyList-style rules are compiled into active tab verdicts"
        );
    }

    #[test]
    fn reload_on_a_crashed_tab_respawns_it() {
        let (session, helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        helper.crash();
        run_panel(&mut state);
        assert!(state.tabs[0].session.is_crashed());

        // The Reload button on a crashed tab requests a respawn; the shell swaps in
        // a fresh session (here a new fake helper) and the new page flows again.
        state.respawn_requested = true;
        assert!(state.take_respawn_request());
        let (fresh, _helper2) = testkit::connect().expect("respawn connect");
        state.respawn_active_with(fresh);
        assert!(
            !state.tabs[0].session.is_crashed(),
            "respawned session is live-ish"
        );
        assert!(
            run_until_texture(&mut state),
            "the respawned tab never uploaded a frame"
        );
    }

    // ── MENUBAR-ALL: the Browser bar dispatches its actions to real seams ───────

    #[test]
    fn the_menu_reload_on_a_live_tab_reloads_without_a_respawn() {
        // The View→Reload item on a live tab drives `WebSession::reload` (the same
        // seam the toolbar Reload button uses) and is NOT a respawn.
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        let ctx = egui::Context::default();
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::Reload);
        assert!(!state.respawn_requested, "a live reload is not a respawn");
        assert!(!state.tabs[0].session.is_crashed());
    }

    #[test]
    fn the_menu_open_address_loads_the_typed_draft_on_the_live_tab() {
        // Page → Open Typed Address drives `WebSession::load` (the toolbar Go
        // button's exact seam). The fake helper answers a Load with a fresh
        // frame + PaintReady, so observing a new frame proves the menu action
        // reached the live session — not a parallel path (§6).
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state); // drain the initial frame
        state.address = "https://example.com/".to_owned();
        let ctx = egui::Context::default();
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::OpenAddress);
        assert!(
            wait_for_fresh_frame(&mut state),
            "OpenAddress reached the helper (a fresh frame arrived for the load)"
        );
    }

    #[test]
    fn the_menu_open_address_uses_the_http_prompt() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.address = "http://plain.example/".to_owned();
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::OpenAddress);

        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://plain.example/")
        );
        assert!(
            !state.tabs[0].session.nav().loading,
            "menu OpenAddress pauses on explicit HTTP just like toolbar Go"
        );
    }

    #[test]
    fn the_privacy_menu_clear_current_tab_returns_to_the_dashboard() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.address = "https://example.com/".to_owned();
        state.submit_address();
        assert!(
            wait_for_fresh_frame(&mut state),
            "precondition: loaded page reached helper"
        );
        state.dashboard_query = "leftover".to_owned();
        state.insecure_prompt = Some("http://plain.example/".to_owned());
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::ClearCurrentTabData,
        );

        assert_eq!(state.address, NEW_TAB_URL);
        assert!(state.dashboard_query.is_empty());
        assert_eq!(state.insecure_prompt, None);
        assert!(
            wait_for_fresh_frame(&mut state),
            "clear action loaded about:blank into the helper"
        );
        assert!(
            run_panel(&mut state),
            "new-tab dashboard renders after clear"
        );
    }

    #[test]
    fn clear_all_browsing_data_forgets_history_downloads_and_reopen_stack() {
        let mut state = WebState::default();
        state
            .history
            .record("https://visited.example/", "Visited", 1000);
        state.closed_tabs.push(ClosedTab {
            url: "https://closed.example/".into(),
            title: "Closed".into(),
            engine: BrowserEngine::Servo,
        });
        // Site data: a saved login + a permission grant must also be forgotten.
        state.save_login("saved.example", "alice", "pw");
        state.grant_permission("https://granted.example", 0);
        assert!(!state.history.is_empty());
        assert_eq!(state.closed_tabs.len(), 1);
        assert_eq!(state.session_logins.len(), 1);
        assert!(state.is_permission_granted("https://granted.example", 0));

        // Drive it through the real Privacy-menu action, not the private method.
        let ctx = egui::Context::default();
        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::ClearAllBrowsingData,
        );

        assert!(state.history.is_empty(), "history forgotten");
        assert!(state.closed_tabs.is_empty(), "reopen stack forgotten");
        assert!(state.session_logins.is_empty(), "saved logins forgotten");
        assert!(
            !state.is_permission_granted("https://granted.example", 0),
            "permission grants forgotten"
        );
        assert_eq!(state.address, NEW_TAB_URL, "returns to the new-tab surface");
    }

    #[test]
    fn the_menu_reload_on_a_crashed_tab_requests_a_respawn() {
        // On a crashed tab the SAME View→Reload item becomes a respawn request —
        // byte-identical to the toolbar Reload's crashed-tab behaviour.
        let (session, helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        helper.crash();
        run_panel(&mut state); // the poll observes the crash
        assert!(state.tabs[0].session.is_crashed());
        let ctx = egui::Context::default();
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::Reload);
        assert!(
            state.respawn_requested,
            "menu Reload on a crashed tab requests a respawn (like the toolbar)"
        );
    }

    // ── BOOKMARKS-10: mesh integration ─────────────────────────────────────────

    #[test]
    fn bookmark_add_body_is_the_workers_add_verb_shape() {
        let body = bookmark_add_body("https://example.com/", "Example Domain");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["url"], "https://example.com/");
        assert_eq!(v["title"], "Example Domain");
        // `source` is omitted so the worker mints the default `Source::Manual`.
        assert!(v.get("source").is_none());
    }

    #[test]
    fn adfilter_domain_body_matches_the_worker_allow_block_shape() {
        assert_eq!(ACTION_ADFILTER_ALLOW, "action/adfilter/allow");
        assert_eq!(ACTION_ADFILTER_BLOCK, "action/adfilter/block");
        let body = adfilter_domain_body(" news.example.com ");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["domain"], "news.example.com");
    }

    #[test]
    fn chat_share_body_round_trips_into_a_clipboard_message_kind() {
        let body = chat_share_body("eagle", "https://example.com/", "Example Domain");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["scope"], "peer");
        assert_eq!(v["to"], "eagle");
        // Prove it's the REAL NOTIFY-CHAT message-kind (snake_case-tagged), not a
        // hand-rolled shape: the `kind` deserializes straight back into MessageKind.
        let kind: MessageKind = serde_json::from_value(v["kind"].clone()).expect("a MessageKind");
        assert!(matches!(kind, MessageKind::Clipboard { .. }));
        assert_eq!(v["kind"]["clipboard"]["preview"], "Example Domain");
        assert_eq!(v["kind"]["clipboard"]["full"], "https://example.com/");
    }

    #[test]
    fn chat_share_preview_falls_back_to_the_url_when_the_title_is_blank() {
        let body = chat_share_body("eagle", "https://example.com/", "   ");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["kind"]["clipboard"]["preview"], "https://example.com/");
    }

    #[test]
    fn browser_share_body_is_the_platform_handoff_shape() {
        let body = browser_share_body(
            BrowserShareTarget::Email,
            "https://example.com/",
            "Example Domain",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_share");
        assert_eq!(v["target"], "email");
        assert_eq!(v["url"], "https://example.com/");
        assert_eq!(v["title"], "Example Domain");
        assert_eq!(v["preview"], "Example Domain");
        assert_eq!(v["source"], "browser");
        assert!(v["host"].as_str().is_some_and(|host| !host.is_empty()));
    }

    #[test]
    fn browser_share_preview_falls_back_to_the_url_when_the_title_is_blank() {
        let body = browser_share_body(BrowserShareTarget::Qr, "https://example.com/", "   ");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["target"], "qr");
        assert_eq!(v["title"], "");
        assert_eq!(v["preview"], "https://example.com/");
    }

    #[test]
    fn browser_send_tab_body_is_the_follow_me_handoff_shape() {
        assert_eq!(ACTION_BROWSER_SEND_TAB, "action/browser/send-tab");
        let body = browser_send_tab_body(
            BrowserSendTabTarget::Phone,
            BrowserEngine::Cef,
            "https://example.com/",
            "Example Domain",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_send_tab");
        assert_eq!(v["target"], "phone");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://example.com/");
        assert_eq!(v["title"], "Example Domain");
        assert_eq!(v["preview"], "Example Domain");
        assert_eq!(v["source"], "browser");
        assert!(v["host"].as_str().is_some_and(|host| !host.is_empty()));
    }

    #[test]
    fn browser_send_tab_preview_falls_back_to_the_url_when_the_title_is_blank() {
        let body = browser_send_tab_body(
            BrowserSendTabTarget::Node,
            BrowserEngine::Servo,
            "https://example.com/",
            "   ",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["target"], "node");
        assert_eq!(v["target_id"], local_hostname());
        assert_eq!(v["target_label"], local_hostname());
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["title"], "");
        assert_eq!(v["preview"], "https://example.com/");
    }

    #[test]
    fn browser_permission_prompt_body_records_default_deny_decision() {
        assert_eq!(
            ACTION_BROWSER_PERMISSION_PROMPT,
            "action/browser/permission-prompt"
        );
        let body = browser_permission_prompt_body(
            DevicePermissionKind::Microphone,
            BrowserEngine::Cef,
            "https://meet.example/",
            "Meeting",
            "meet.example",
            123,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_permission_prompt");
        assert_eq!(v["permission"], "microphone");
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["enforcement"], "helper_default_deny");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://meet.example/");
        assert_eq!(v["title"], "Meeting");
        assert_eq!(v["site"], "meet.example");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["updated_ms"], 123);
    }

    #[test]
    fn browser_power_mode_view_source_opens_source_in_a_foreground_tab() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::TogglePowerMode,
        );
        assert!(state.power_mode);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::OpenViewSource);

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "view-source:https://example.test/".to_owned(),
            })
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Power mode: opening page source")
        );
    }

    #[test]
    fn browser_power_mode_device_permission_prompt_records_default_deny_handoff() {
        let (session, _helper, _writer) = live_page_session();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        state.power_mode = true;
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::PromptCameraPermission,
        );

        assert!(
            state
                .active_site_permission_summary()
                .is_some_and(|summary| summary
                    .contains("example.test: camera denied; helper default deny remains active")),
            "prompt history should be reflected in the active-site permission summary"
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Camera prompt denied for example.test; helper default deny remains active")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let entries = persist
            .list_since(ACTION_BROWSER_PERMISSION_PROMPT, None)
            .expect("read permission prompt action");
        assert_eq!(entries.len(), 1);
        let body = entries[0].body.as_deref().expect("permission prompt body");
        let v: serde_json::Value = serde_json::from_str(body).expect("permission prompt body JSON");
        assert_eq!(v["op"], "browser_permission_prompt");
        assert_eq!(v["permission"], "camera");
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["enforcement"], "helper_default_deny");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["site"], "example.test");
        assert_eq!(v["url"], "https://example.test/");
    }

    #[test]
    fn chromium_devtools_target_json_selects_active_page_frontend() {
        let body = serde_json::json!([
            {
                "type": "page",
                "url": "https://other.example/",
                "devtoolsFrontendUrl": "/devtools/inspector.html?ws=127.0.0.1:9222/devtools/page/OTHER",
                "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/OTHER"
            },
            {
                "type": "page",
                "url": "https://example.test/app",
                "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/ACTIVE"
            }
        ])
        .to_string();

        let selected =
            chromium_devtools_frontend_from_list("https://example.test/app", &body).unwrap();

        assert_eq!(
            selected.as_deref(),
            Some("http://127.0.0.1:9222/devtools/inspector.html?ws=127.0.0.1:9222/devtools/page/ACTIVE")
        );
        let fallback =
            chromium_devtools_frontend_from_list("https://missing.example/", &body).unwrap();
        assert_eq!(
            fallback.as_deref(),
            Some("http://127.0.0.1:9222/devtools/inspector.html?ws=127.0.0.1:9222/devtools/page/OTHER")
        );
    }

    #[test]
    fn browser_power_mode_chromium_devtools_opens_cef_debug_portal() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.power_mode = true;
        state.tabs[state.active].engine = BrowserEngine::Cef;
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::OpenChromiumDevtools,
        );

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: CEF_DEVTOOLS_URL.to_owned(),
            })
        );
        assert!(
            state
                .capture_notice
                .as_deref()
                .is_some_and(|notice| notice
                    .starts_with("Power mode: opening Chromium DevTools target list")),
            "unexpected DevTools notice: {:?}",
            state.capture_notice
        );

        state.tabs[state.active].engine = BrowserEngine::Servo;
        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::OpenChromiumDevtools,
        );
        assert_eq!(state.take_open_request(), None);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Chromium DevTools requires a live CEF tab")
        );
    }

    // ── DD-2: the compact toolbar's Stop control stays CEF-only ────────────────
    //
    // Servo's embedding API (the pinned `servo`/`servo-embedder-traits`/
    // `servo-constellation-traits` 0.3.0 crates.io publications) exposes no
    // stop/cancel-navigation primitive anywhere in its reachable surface
    // (investigated 2026-07-10 — see `can_show_stop_control`'s doc comment).
    // These lock in the honest degrade: a loading Servo tab must never present
    // a Stop control that would silently do nothing when clicked.

    #[test]
    fn a_loading_cef_tab_shows_a_real_stop_control() {
        assert!(
            can_show_stop_control(true, false, true, Some(BrowserEngine::Cef)),
            "CEF has a real cef_browser_t::stop_load hook, so Stop must be offered"
        );
    }

    #[test]
    fn a_loading_servo_tab_never_shows_a_fake_stop_control() {
        assert!(
            !can_show_stop_control(true, false, true, Some(BrowserEngine::Servo)),
            "Servo exposes no cancel-load hook (DD-2 2026-07-10) — a Stop button \
             here would do nothing when clicked, so it must stay honest Reload"
        );
    }

    #[test]
    fn stop_control_still_requires_a_live_loading_uncrashed_tab() {
        // Even for CEF, Stop is gated on every other precondition: no tab, no
        // in-flight load, and a crashed tab (which shows a respawn Reload
        // instead) must all fall back to the honest Reload control too.
        assert!(
            !can_show_stop_control(false, false, true, Some(BrowserEngine::Cef)),
            "no tab ⇒ no Stop"
        );
        assert!(
            !can_show_stop_control(true, false, false, Some(BrowserEngine::Cef)),
            "not loading ⇒ no Stop"
        );
        assert!(
            !can_show_stop_control(true, true, true, Some(BrowserEngine::Cef)),
            "a crashed tab shows a respawn Reload, never Stop"
        );
        assert!(
            !can_show_stop_control(true, false, true, None),
            "no active engine ⇒ no Stop"
        );
    }

    #[test]
    fn browser_session_sync_body_carries_tabs_settings_and_downloads() {
        assert_eq!(ACTION_BROWSER_SESSION_SYNC, "action/browser/session-sync");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.tabs[state.active].container = ContainerProfile::Work;
        state.tabs[state.active].display_target = DisplayTarget::Secondary;
        state.tabs[state.active].muted = true;
        state.tabs[state.active].force_dark = true;
        state.tabs[state.active].user_scripts = true;
        state.tabs[state.active].user_agent = UserAgentOverride::AndroidChrome;
        state.tabs[state.active].device_profile = DeviceProfile::Phone;
        state.vertical_tabs = true;
        state.page_zoom_percent = 125;
        state.downloads_open = true;
        state.power_mode = true;
        state.speed_dial = vec![SpeedDialEntry::new(
            "Ops",
            "https://ops.mesh/",
            "Open the mesh ops console",
        )];
        let mut job = browser_output_transfer_job("/tmp/source.bin", "/tmp/dest.bin");
        job.state = TransferState::Running;
        job.progress = Some(42);
        state.download_jobs.push(job);

        let body = browser_session_sync_body(&state);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_session_sync");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["active_index"], 0);
        assert_eq!(v["settings"]["vertical_tabs"], true);
        assert_eq!(v["settings"]["page_zoom_percent"], 125);
        assert_eq!(v["settings"]["downloads_open"], true);
        assert_eq!(v["settings"]["power_mode"], true);
        assert_eq!(v["settings"]["speed_dial"][0]["label"], "Ops");
        assert_eq!(v["settings"]["speed_dial"][0]["url"], "https://ops.mesh/");
        assert_eq!(
            v["settings"]["speed_dial"][0]["hint"],
            "Open the mesh ops console"
        );
        assert_eq!(v["tabs"][0]["engine"], "servo");
        assert_eq!(v["tabs"][0]["container"], "work");
        assert_eq!(v["tabs"][0]["display_target"], "secondary");
        assert_eq!(v["tabs"][0]["url"], "https://example.test/");
        assert_eq!(v["tabs"][0]["muted"], true);
        assert_eq!(v["tabs"][0]["force_dark"], true);
        assert_eq!(v["tabs"][0]["user_scripts"], true);
        assert_eq!(v["tabs"][0]["user_agent"], "android_chrome");
        assert_eq!(v["tabs"][0]["device_profile"], "phone");
        assert_eq!(v["downloads"][0]["method"], "browser_download");
        assert_eq!(v["downloads"][0]["state"], "running");
        assert_eq!(v["downloads"][0]["progress"], 42);
    }

    #[test]
    fn browser_session_restore_enqueues_tabs_with_the_active_tab_last() {
        let body = serde_json::json!({
            "op": "browser_session_sync",
            "active_index": 1,
            "settings": {
                "future_engine": "cef",
                "vertical_tabs": true,
                "page_zoom_percent": 135,
                "find_open": true,
                "downloads_open": true,
                "power_mode": true,
                "speed_dial": [
                    {"label": "Ops", "url": "https://ops.mesh/", "hint": "Open ops"},
                    {"label": "", "url": "https://drop.example/", "hint": "drop"},
                    {"label": "No URL", "url": "", "hint": "drop"}
                ],
            },
            "tabs": [
                {"index": 0, "engine": "servo", "url": "https://first.example/"},
                {"index": 1, "engine": "cef", "url": "https://active.example/"},
                {"index": 2, "engine": "servo", "url": "https://last.example/"},
            ],
            "downloads": [],
        })
        .to_string();
        let mut state = WebState::default();

        let restored = state
            .restore_session_sync_snapshot(&body)
            .expect("restore snapshot");

        assert_eq!(restored, 3);
        assert_eq!(state.engine, BrowserEngine::Cef);
        assert!(state.vertical_tabs);
        assert_eq!(state.page_zoom_percent, 135);
        assert!(state.find_open);
        assert!(state.downloads_open);
        assert!(state.power_mode);
        assert_eq!(
            state.speed_dial,
            vec![SpeedDialEntry::new("Ops", "https://ops.mesh/", "Open ops")]
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "https://first.example/".to_owned(),
            })
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "https://last.example/".to_owned(),
            })
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://active.example/".to_owned(),
            })
        );
        assert_eq!(state.take_open_request(), None);
    }

    #[test]
    fn browser_session_restore_rejects_the_wrong_snapshot_shape() {
        let mut state = WebState::default();
        assert!(state.restore_session_sync_snapshot("{}").is_err());
        assert!(state
            .restore_session_sync_snapshot(r#"{"op":"browser_send_tab","tabs":[]}"#)
            .is_err());
    }

    #[test]
    fn browser_startup_restore_reads_daemon_latest_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let path = session_sync_latest_path(root.path(), &host);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "op": "browser_session_sync",
                "active_index": 0,
                "settings": {
                    "future_engine": "cef",
                    "speed_dial": [
                        {"label": "Ops", "url": "https://ops.mesh/", "hint": "Open ops"}
                    ],
                },
                "tabs": [
                    {"index": 0, "engine": "cef", "url": "https://restored.mesh/"}
                ],
                "downloads": [],
            })
            .to_string(),
        )
        .unwrap();
        let mut state =
            WebState::default().with_session_restore_roots(vec![root.path().to_path_buf()]);

        assert_eq!(state.restore_startup_session_once(), Some(1));

        assert_eq!(state.engine, BrowserEngine::Cef);
        assert_eq!(
            state.speed_dial,
            vec![SpeedDialEntry::new("Ops", "https://ops.mesh/", "Open ops")]
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://restored.mesh/".to_owned(),
            })
        );
        assert_eq!(state.restore_startup_session_once(), None);
    }

    #[test]
    fn browser_startup_restore_host_path_matches_the_daemon_sanitizer() {
        assert_eq!(sanitize_session_host("work station/1"), "work-station1");
        assert_eq!(
            session_sync_latest_path(Path::new("/mesh"), "work station/1"),
            PathBuf::from("/mesh/browser-session-sync/work-station1/latest.json")
        );
        assert_eq!(
            send_tab_inbox_dir(Path::new("/mesh"), "work station/1"),
            PathBuf::from("/mesh/browser-send-tab/node/work-station1")
        );
    }

    #[test]
    fn browser_send_tab_outbox_enqueues_local_node_tabs_and_unlinks_records() {
        let root = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let path = send_tab_inbox_dir(root.path(), &host)
            .join("source-node")
            .join("01Send.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "op": "browser_send_tab",
                "target": "node",
                "target_id": host,
                "target_label": host,
                "engine": "cef",
                "url": "https://handoff.mesh/",
                "title": "Handoff",
                "preview": "Handoff",
                "source": "browser",
                "host": "source-node"
            })
            .to_string(),
        )
        .unwrap();
        let mut state =
            WebState::default().with_session_restore_roots(vec![root.path().to_path_buf()]);

        assert_eq!(state.drain_incoming_send_tabs(), 1);

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://handoff.mesh/".to_owned(),
            })
        );
        assert!(
            !path.exists(),
            "consumed send-tab records are unlinked so they do not replay"
        );
    }

    #[test]
    fn browser_send_tab_outbox_rejects_phone_and_other_node_records() {
        let root = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let local_dir = send_tab_inbox_dir(root.path(), &host).join("source-node");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(
            local_dir.join("phone.json"),
            serde_json::json!({
                "op": "browser_send_tab",
                "target": "phone",
                "target_id": host,
                "engine": "cef",
                "url": "https://phone.mesh/"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            local_dir.join("other.json"),
            serde_json::json!({
                "op": "browser_send_tab",
                "target": "node",
                "target_id": "other-node",
                "engine": "servo",
                "url": "https://other.mesh/"
            })
            .to_string(),
        )
        .unwrap();
        let mut state =
            WebState::default().with_session_restore_roots(vec![root.path().to_path_buf()]);

        assert_eq!(state.drain_incoming_send_tabs(), 0);
        assert_eq!(state.take_open_request(), None);
        assert!(local_dir.join("phone.json").exists());
        assert!(local_dir.join("other.json").exists());
    }

    #[test]
    fn browser_send_tab_outbox_dedupes_local_and_shared_records() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let body = serde_json::json!({
            "op": "browser_send_tab",
            "target": "node",
            "target_id": host,
            "engine": "servo",
            "url": "https://dedupe.mesh/",
            "host": "source-node"
        })
        .to_string();
        let local_path = send_tab_inbox_dir(local.path(), &host)
            .join("source-node")
            .join("01Same.json");
        let share_path = send_tab_inbox_dir(share.path(), &host)
            .join("source-node")
            .join("01Same.json");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(share_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, &body).unwrap();
        std::fs::write(&share_path, &body).unwrap();
        let mut state = WebState::default().with_session_restore_roots(vec![
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        ]);

        assert_eq!(state.drain_incoming_send_tabs(), 1);

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "https://dedupe.mesh/".to_owned(),
            })
        );
        assert_eq!(state.take_open_request(), None);
        assert!(!local_path.exists());
        assert!(!share_path.exists());
    }

    #[test]
    fn omnibox_resolves_urls_hosts_and_searches() {
        assert_eq!(
            omnibox_target(" https://example.com/a "),
            Some("https://example.com/a".to_owned())
        );
        assert_eq!(
            omnibox_target("about:blank"),
            Some("about:blank".to_owned())
        );
        assert_eq!(
            omnibox_target("data:text/html,hi"),
            Some("data:text/html,hi".to_owned())
        );
        assert_eq!(
            omnibox_target("example.com"),
            Some("https://example.com".to_owned())
        );
        assert_eq!(
            omnibox_target("localhost:8080/admin"),
            Some("https://localhost:8080/admin".to_owned())
        );
        assert_eq!(
            omnibox_target("10.42.0.1:4533"),
            Some("https://10.42.0.1:4533".to_owned())
        );
        assert_eq!(
            omnibox_target("mesh browser status"),
            Some("https://search.mesh/search?q=mesh+browser+status".to_owned())
        );
        assert_eq!(
            omnibox_target("a+b & c"),
            Some("https://search.mesh/search?q=a%2Bb+%26+c".to_owned())
        );
        assert_eq!(omnibox_target("  "), None);
    }

    // ── OMNIBOX-STYLE — Chrome-style omnibox display + security chip ───────────

    #[test]
    fn omnibox_display_elides_https_and_strips_www() {
        let display = omnibox_display("https://www.example.com/x");
        assert_eq!(display.scheme_shown, None, "https:// is elided");
        assert_eq!(display.host, "example.com");
        assert_eq!(display.host_emphasis, 0..display.host.len());
        assert_eq!(&display.host[display.host_emphasis.clone()], "example.com");
        assert_eq!(display.rest, "/x");
        assert_eq!(display.security, SecurityLevel::Secure);
    }

    #[test]
    fn omnibox_display_keeps_http_scheme_as_a_downgrade_signal() {
        let display = omnibox_display("http://example.com");
        assert_eq!(display.scheme_shown, Some("http://".to_owned()));
        assert_eq!(display.host, "example.com");
        assert_eq!(display.rest, "");
        assert_eq!(display.security, SecurityLevel::NotSecure);
    }

    #[test]
    fn omnibox_display_treats_mesh_scheme_as_trusted() {
        let display = omnibox_display("mesh://music.mesh");
        assert_eq!(display.scheme_shown, Some("mesh://".to_owned()));
        assert_eq!(display.host, "music.mesh");
        assert_eq!(display.security, SecurityLevel::Mesh);
    }

    #[test]
    fn omnibox_display_emphasizes_the_registrable_domain_under_a_subdomain() {
        let display = omnibox_display("https://foo.bar.example.com/p");
        assert_eq!(display.host, "foo.bar.example.com");
        assert_eq!(&display.host[display.host_emphasis.clone()], "example.com");
        assert_eq!(display.rest, "/p");
    }

    #[test]
    fn omnibox_display_emphasizes_a_full_two_level_suffix_registrable_domain() {
        let display = omnibox_display("https://foo.co.uk/p");
        assert_eq!(display.host, "foo.co.uk");
        assert_eq!(&display.host[display.host_emphasis.clone()], "foo.co.uk");
        assert_eq!(display.rest, "/p");
    }

    #[test]
    fn omnibox_display_neutral_scheme_stays_unmodified() {
        let display = omnibox_display("about:blank");
        assert_eq!(display.scheme_shown, Some("about:blank".to_owned()));
        assert_eq!(display.host, "");
        assert_eq!(display.security, SecurityLevel::Neutral);
    }

    #[test]
    fn omnibox_display_empty_url_shown_as_neutral_with_no_scheme() {
        let display = omnibox_display("   ");
        assert_eq!(display.scheme_shown, None);
        assert_eq!(display.host, "");
        assert_eq!(display.security, SecurityLevel::Neutral);
    }

    #[test]
    fn omnibox_layout_job_covers_the_full_text_for_an_elided_https_url() {
        let font_id = egui::FontId::new(CHROME_FONT, egui::FontFamily::Proportional);
        let job = omnibox_layout_job("https://www.example.com/x", font_id);
        // The elided job's text is shorter than the raw address (no `https://`,
        // no `www.`) — that mismatch is exactly why the styled read-out is
        // painted as an overlay rather than fed into the TextEdit's own
        // layouter (which must stay 1:1 with the buffer for cursor mapping).
        assert_eq!(job.text, "example.com/x");
    }

    #[test]
    fn security_level_tones_use_design_tokens_not_raw_literals() {
        assert_eq!(SecurityLevel::Secure.tone().color(), Style::TEXT_DIM);
        assert_eq!(SecurityLevel::NotSecure.tone().color(), Style::WARN);
        assert_eq!(SecurityLevel::Mesh.tone().color(), Style::ACCENT);
        assert_eq!(SecurityLevel::Neutral.tone().color(), Style::TEXT_DIM);
    }

    // ── SECURITY-INFO — the site-info panel opened by the security chip ────────

    #[test]
    fn security_headline_maps_each_level_to_plain_language_copy() {
        assert_eq!(
            security_headline(SecurityLevel::Secure),
            "Connection is secure"
        );
        assert_eq!(
            security_headline(SecurityLevel::NotSecure),
            "Your connection to this site is not secure"
        );
        assert_eq!(
            security_headline(SecurityLevel::Mesh),
            "Mesh service \u{2014} trusted overlay"
        );
        assert_eq!(security_headline(SecurityLevel::Neutral), "About this page");
    }

    #[test]
    fn site_info_summary_host_matches_the_omnibox_displays_host_and_emphasis() {
        let url = "https://foo.example.com/x";
        let display = omnibox_display(url);
        let summary = site_info_summary(url);
        assert_eq!(summary.host, display.host);
        assert_eq!(summary.host_emphasis, display.host_emphasis);
        assert_eq!(summary.host, "foo.example.com");
        assert_eq!(&summary.host[summary.host_emphasis.clone()], "example.com");
    }

    #[test]
    fn site_info_summary_surfaces_idn_homograph_warning() {
        // A punycode/IDN host (xn-- prefix) trips the spoofing warning...
        assert!(
            site_info_summary("https://xn--pple-43d.com/")
                .confusable
                .is_some(),
            "punycode host warns"
        );
        // ...a look-alike Cyrillic 'а' (U+0430) mixed with Latin trips it too...
        assert!(confusable_warning("\u{0430}pple.com").is_some());
        // ...and a plain ASCII host does not.
        assert!(site_info_summary("https://example.com/")
            .confusable
            .is_none());
        assert!(confusable_warning("apple.com").is_none());
    }

    #[test]
    fn site_info_summary_shows_a_cert_line_only_for_https() {
        assert!(site_info_summary("https://example.com/")
            .cert_line
            .is_some());
        assert!(site_info_summary("http://example.com/").cert_line.is_none());
        assert!(site_info_summary("mesh://svc.mesh/").cert_line.is_none());
        assert!(site_info_summary("about:blank").cert_line.is_none());
    }

    #[test]
    fn site_info_panel_opens_from_the_security_chip_and_renders_without_panicking() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        // Establish the live page (https://example.test/) in the chrome bar.
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        // Force the popup open the same way the chip's click handler does —
        // `security_chip_popup_id` is a fixed key, not a ui-path-derived one,
        // so the test doesn't need to replay the chrome bar's exact layout.
        ctx.memory_mut(|mem| mem.open_popup(security_chip_popup_id()));
        // A second frame with the panel open must still paint, not panic.
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert!(ctx.memory(|mem| mem.is_popup_open(security_chip_popup_id())));
    }

    #[test]
    fn external_tel_urls_handoff_to_voice_without_helper_navigation() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        state.address = "tel:+15551234567".to_owned();
        state.submit_address();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_VOICE_DIAL, None)
            .expect("list voice dial actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("dial body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["peer"], "+15551234567");
        assert_eq!(v["host"], local_hostname());
        assert_eq!(v["source"], "browser");
        assert_eq!(v["url"], "tel:+15551234567");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Handed tel link to Voice")
        );

        let controls = drain_control_messages(&helper);
        assert!(
            controls
                .iter()
                .all(|msg| !matches!(msg, mde_web_preview_client::ControlMsg::Load(_))),
            "external protocol handoff must not navigate the helper: {controls:?}"
        );
    }

    #[test]
    fn mailto_and_magnet_urls_publish_browser_protocol_handoffs() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        state.address = "mailto:ops@example.test?subject=mesh".to_owned();
        state.submit_address();
        state.address = "magnet:?xt=urn:btih:0123456789abcdef".to_owned();
        state.submit_address();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_PROTOCOL, None)
            .expect("list browser protocol actions");
        assert_eq!(msgs.len(), 2);

        let mail: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("mailto body"))
                .expect("valid JSON");
        assert_eq!(mail["op"], "browser_protocol_handoff");
        assert_eq!(mail["source"], "browser");
        assert_eq!(mail["host"], local_hostname());
        assert_eq!(mail["scheme"], "mailto");
        assert_eq!(mail["target"], "email");
        assert_eq!(mail["url"], "mailto:ops@example.test?subject=mesh");

        let magnet: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("magnet body"))
                .expect("valid JSON");
        assert_eq!(magnet["scheme"], "magnet");
        assert_eq!(magnet["target"], "transfers");
        assert_eq!(magnet["source"], "browser");
        assert_eq!(magnet["host"], local_hostname());
        assert_eq!(magnet["url"], "magnet:?xt=urn:btih:0123456789abcdef");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Handed magnet link to Transfers")
        );

        let controls = drain_control_messages(&helper);
        assert!(
            controls
                .iter()
                .all(|msg| !matches!(msg, mde_web_preview_client::ControlMsg::Load(_))),
            "external protocol handoff must not navigate the helper: {controls:?}"
        );
    }

    #[test]
    fn browser_share_menu_actions_publish_platform_handoffs() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToPeer);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToPhone);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToEmail);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToQr);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_SHARE, None)
            .expect("list browser share actions");
        assert_eq!(msgs.len(), 4);
        let targets: Vec<String> = msgs
            .iter()
            .map(|msg| {
                let body = msg.body.as_deref().expect("share body");
                let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
                assert_eq!(v["op"], "browser_share");
                assert_eq!(v["source"], "browser");
                assert_eq!(v["url"], "https://example.test/");
                v["target"].as_str().expect("target").to_owned()
            })
            .collect();
        assert_eq!(targets, ["peer", "phone", "email", "qr"]);
    }

    #[test]
    fn browser_share_route_result_parser_accepts_daemon_qr_routes() {
        let body = serde_json::json!({
            "op": "browser_share_routed",
            "source": "browser_share",
            "node": local_hostname(),
            "request_id": "01HSHARE",
            "host": local_hostname(),
            "target": "qr",
            "url": "https://example.test/share",
            "title": "Example",
            "preview": "Example",
            "routed_ms": 123,
            "updated_ms": 123,
        })
        .to_string();

        let route = parse_share_route_result(&body).expect("valid share route");
        assert_eq!(route.host, local_hostname());
        assert_eq!(route.target, BrowserShareTarget::Qr);
        assert_eq!(route.url, "https://example.test/share");
        let qr = qr_share_result(route).expect("QR route renders");
        assert!(
            qr.modules.len() >= 21,
            "a real QR matrix is generated, not a placeholder"
        );
        assert!(
            qr.modules.iter().flatten().any(|dark| *dark),
            "QR matrix has dark modules"
        );

        let bad_source = body.replace("browser_share", "cloud_share");
        assert!(parse_share_route_result(&bad_source).is_err());
        let bad_target = body.replace(r#""target":"qr""#, r#""target":"fax""#);
        assert!(parse_share_route_result(&bad_target).is_err());
    }

    #[test]
    fn browser_qr_share_results_are_displayed_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_share_result_topic(&local_hostname());
        let peer_body = serde_json::json!({
            "op": "browser_share_routed",
            "source": "browser_share",
            "node": local_hostname(),
            "request_id": "01HPEER",
            "host": local_hostname(),
            "target": "peer",
            "url": "https://example.test/peer",
            "title": "Peer",
            "preview": "Peer",
            "routed_ms": 123,
            "updated_ms": 123,
        })
        .to_string();
        let qr_body = serde_json::json!({
            "op": "browser_share_routed",
            "source": "browser_share",
            "node": local_hostname(),
            "request_id": "01HQR",
            "host": local_hostname(),
            "target": "qr",
            "url": "https://example.test/qr",
            "title": "QR",
            "preview": "QR",
            "routed_ms": 124,
            "updated_ms": 124,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&peer_body))
            .expect("write peer share result");
        persist
            .write(&topic, Priority::Default, None, Some(&qr_body))
            .expect("write qr share result");

        state.poll_share_results();
        let latest = state.latest_qr_share.as_ref().expect("QR share displayed");
        assert_eq!(latest.url, "https://example.test/qr");
        assert_eq!(latest.request_id, "01HQR");
        assert_eq!(state.capture_notice.as_deref(), Some("QR share ready"));

        state.latest_qr_share = None;
        state.share_result_last_poll = None;
        state.poll_share_results();
        assert!(
            state.latest_qr_share.is_none(),
            "cursor prevents replaying the same QR share event"
        );
    }

    #[test]
    fn browser_send_tab_menu_actions_publish_follow_me_handoffs() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.tabs[state.active].engine = BrowserEngine::Cef;
        run_until_texture(&mut state);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::SendTabToNode);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::SendTabToPhone);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_SEND_TAB, None)
            .expect("list browser send-tab actions");
        assert_eq!(msgs.len(), 2);
        let targets: Vec<String> = msgs
            .iter()
            .map(|msg| {
                let body = msg.body.as_deref().expect("send-tab body");
                let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
                assert_eq!(v["op"], "browser_send_tab");
                assert_eq!(v["source"], "browser");
                assert_eq!(v["engine"], "cef");
                assert_eq!(v["url"], "https://example.test/");
                if v["target"] == "node" {
                    assert_eq!(v["target_id"], local_hostname());
                    assert_eq!(v["target_label"], local_hostname());
                }
                v["target"].as_str().expect("target").to_owned()
            })
            .collect();
        assert_eq!(targets, ["node", "phone"]);
    }

    #[test]
    fn browser_read_aloud_requests_page_text_and_publishes_tts_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ReadAloud);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Read aloud: reading page text")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "read aloud must request bounded page text from the helper: {controls:?}"
        );

        state.handle_page_text_event(1, "  Hello from the page.  ".to_owned());
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Read aloud: sent page text to TTS")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_READ_ALOUD, None)
            .expect("list browser read-aloud actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("read-aloud body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_read_aloud");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");
        assert_eq!(v["text"], "Hello from the page.");
        assert_eq!(v["text_chars"], 20);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn browser_read_aloud_body_clamps_page_text_for_the_bus() {
        let request = ReadAloudRequest {
            tab_index: 3,
            engine: BrowserEngine::Cef,
            url: "https://long.example/".to_owned(),
            title: "Long".to_owned(),
        };
        let body = browser_read_aloud_body(
            &request,
            &format!("{}tail", "x".repeat(READ_ALOUD_TEXT_MAX_CHARS)),
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_read_aloud");
        assert_eq!(v["engine"], "cef");
        assert_eq!(
            v["text"].as_str().expect("text").chars().count(),
            READ_ALOUD_TEXT_MAX_CHARS
        );
        assert_eq!(
            v["text_chars"],
            u64::try_from(READ_ALOUD_TEXT_MAX_CHARS).expect("fits")
        );
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn browser_translate_requests_page_text_and_publishes_private_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::TranslatePage);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Translate: reading page text")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "translate must request bounded page text from the helper: {controls:?}"
        );

        state.handle_page_text_event(1, "  Bonjour from the page.  ".to_owned());
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Translate: sent page text to translation")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_TRANSLATE, None)
            .expect("list browser translate actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("translate body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_translate");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");
        assert_eq!(v["source_lang"], "auto");
        assert_eq!(v["target_lang"], "en");
        assert_eq!(v["text"], "Bonjour from the page.");
        assert_eq!(v["text_chars"], 22);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn browser_translate_body_clamps_page_text_for_the_bus() {
        let request = TranslateRequest {
            tab_index: 2,
            engine: BrowserEngine::Cef,
            url: "https://long.example/".to_owned(),
            title: "Long".to_owned(),
            source_lang: "auto".to_owned(),
            target_lang: "es".to_owned(),
        };
        let body = browser_translate_body(
            &request,
            &format!("{}tail", "x".repeat(TRANSLATE_TEXT_MAX_CHARS)),
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_translate");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["target_lang"], "es");
        assert_eq!(
            v["text"].as_str().expect("text").chars().count(),
            TRANSLATE_TEXT_MAX_CHARS
        );
        assert_eq!(
            v["text_chars"],
            u64::try_from(TRANSLATE_TEXT_MAX_CHARS).expect("fits")
        );
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn browser_offline_cache_requests_page_text_and_publishes_private_snapshot() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        let ctx = egui::Context::default();
        let pdf_dir = tempfile::tempdir().expect("pdf fixture dir");
        let pdf_path = pdf_dir.path().join("mde-browser-current.pdf");
        std::fs::write(&pdf_path, b"%PDF-1.7\n% offline cache fixture\n").expect("pdf fixture");
        state.last_saved_pdf = Some(SavedPdf {
            path: pdf_path,
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
        });
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 77,
                url: "https://example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 78,
                url: "https://www.google-analytics.com/collect".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::SaveOfflineCopy,
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Offline cache: reading page text")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "offline cache must request bounded page text from the helper: {controls:?}"
        );

        state.handle_page_text_event(1, "  Cached page body.  ".to_owned());
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Offline cache: saved page snapshot")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_OFFLINE_CACHE, None)
            .expect("list browser offline-cache actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("offline-cache body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_offline_cache");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");
        assert_eq!(v["text"], "Cached page body.");
        assert_eq!(v["text_chars"], 17);
        assert_eq!(v["truncated"], false);
        let viewport = v["viewport_image"]
            .as_object()
            .expect("offline cache carries viewport image");
        assert_eq!(viewport["mime"], "image/png");
        assert_eq!(viewport["width"], testkit::FAKE_W);
        assert_eq!(viewport["height"], testkit::FAKE_H);
        let viewport_bytes = base64::engine::general_purpose::STANDARD
            .decode(viewport["data"].as_str().expect("viewport data"))
            .expect("viewport base64 decodes");
        assert!(viewport_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let archive = v["archive_mhtml"]
            .as_object()
            .expect("offline cache carries MHTML archive");
        assert_eq!(archive["mime"], "multipart/related");
        assert!(archive["filename"]
            .as_str()
            .expect("archive filename")
            .ends_with(".mhtml"));
        let archive_bytes = base64::engine::general_purpose::STANDARD
            .decode(archive["data"].as_str().expect("archive data"))
            .expect("archive base64 decodes");
        assert_eq!(
            archive["bytes"].as_u64().expect("archive bytes") as usize,
            archive_bytes.len()
        );
        let archive_text = String::from_utf8(archive_bytes).expect("archive is utf8");
        assert!(archive_text.starts_with("MIME-Version: 1.0\r\n"));
        assert!(archive_text.contains("multipart/related"));
        assert!(archive_text.contains("Cached page body."));
        let pdf = v["pdf_snapshot"]
            .as_object()
            .expect("offline cache carries current-page PDF snapshot");
        assert_eq!(pdf["mime"], "application/pdf");
        assert_eq!(pdf["filename"], "mde-browser-current.pdf");
        let pdf_bytes = base64::engine::general_purpose::STANDARD
            .decode(pdf["data"].as_str().expect("pdf data"))
            .expect("pdf base64 decodes");
        assert_eq!(
            pdf["bytes"].as_u64().expect("pdf bytes") as usize,
            pdf_bytes.len()
        );
        assert!(pdf_bytes.starts_with(b"%PDF-"));
        let resources = v["resource_manifest"]
            .as_array()
            .expect("offline cache carries resource manifest");
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0]["url"], "https://example.test/app.js");
        assert_eq!(resources[0]["resource"], "script");
        assert_eq!(resources[0]["allowed"], true);
        assert_eq!(
            resources[1]["url"],
            "https://www.google-analytics.com/collect"
        );
        assert_eq!(resources[1]["resource"], "script");
        assert_eq!(resources[1]["allowed"], false);
    }

    #[test]
    fn browser_offline_cache_body_clamps_page_text_for_the_bus() {
        let request = OfflineCacheRequest {
            tab_index: 4,
            engine: BrowserEngine::Cef,
            url: "https://long.example/".to_owned(),
            title: "Long".to_owned(),
            viewport: None,
            resources: Vec::new(),
            pdf_snapshot: None,
        };
        let body = browser_offline_cache_body(
            &request,
            &format!("{}tail", "x".repeat(OFFLINE_CACHE_TEXT_MAX_CHARS)),
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_offline_cache");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "cef");
        assert_eq!(
            v["text"].as_str().expect("text").chars().count(),
            OFFLINE_CACHE_TEXT_MAX_CHARS
        );
        assert_eq!(
            v["text_chars"],
            u64::try_from(OFFLINE_CACHE_TEXT_MAX_CHARS).expect("fits")
        );
        assert_eq!(v["truncated"], true);
        let archive = v["archive_mhtml"].as_object().expect("MHTML archive");
        let archive_bytes = base64::engine::general_purpose::STANDARD
            .decode(archive["data"].as_str().expect("archive data"))
            .expect("archive base64 decodes");
        assert!(
            archive_bytes.len() <= OFFLINE_CACHE_MHTML_MAX_BYTES,
            "archive is bounded"
        );
    }

    #[test]
    fn browser_offline_cache_result_parser_is_private_and_bounded() {
        let viewport = offline_cache_viewport_image(&egui::ColorImage {
            size: [1, 1],
            pixels: vec![egui::Color32::RED],
        })
        .expect("small viewport encodes");
        let archive_request = OfflineCacheRequest {
            tab_index: 2,
            engine: BrowserEngine::Cef,
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
            viewport: Some(viewport.clone()),
            resources: Vec::new(),
            pdf_snapshot: None,
        };
        let archive = offline_cache_mhtml_archive(&archive_request, "Cached archive body", 123)
            .expect("archive encodes");
        let pdf_bytes = b"%PDF-1.7\n% cached parser fixture\n";
        let pdf_data = base64::engine::general_purpose::STANDARD.encode(pdf_bytes);
        let body = serde_json::json!({
            "op": "browser_offline_cache_record",
            "source": "browser_offline_cache",
            "node": local_hostname(),
            "cache_id": "cache-123",
            "host": local_hostname(),
            "privacy": "offline_or_mesh_only",
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "text": format!("{}tail", "x".repeat(OFFLINE_CACHE_TEXT_MAX_CHARS)),
            "text_chars": OFFLINE_CACHE_TEXT_MAX_CHARS + 4,
            "viewport_image": {
                "mime": viewport.mime,
                "width": viewport.width,
                "height": viewport.height,
                "data": viewport.data_base64,
            },
            "archive_mhtml": {
                "mime": archive.mime,
                "filename": archive.filename,
                "bytes": archive.bytes,
                "data": archive.data_base64,
            },
            "resource_manifest": [
                {
                    "url": "https://example.test/app.js",
                    "resource": "script",
                    "allowed": true,
                },
                {
                    "url": "https://tracker.example/pixel.gif",
                    "resource": "image",
                    "allowed": false,
                }
            ],
            "pdf_snapshot": {
                "mime": "application/pdf",
                "filename": "mde-browser-123-example.test.pdf",
                "bytes": pdf_bytes.len(),
                "data": pdf_data.clone(),
            },
            "cached_ms": 123,
        })
        .to_string();
        let result = parse_offline_cache_result(&body).expect("valid offline-cache result");
        assert_eq!(result.host, local_hostname());
        assert_eq!(result.cache_id, "cache-123");
        assert_eq!(result.tab_index, 2);
        assert_eq!(result.engine, BrowserEngine::Cef);
        assert_eq!(result.cached_ms, Some(123));
        let viewport = result.viewport.as_ref().expect("viewport image retained");
        assert_eq!(viewport.mime, "image/png");
        assert_eq!((viewport.width, viewport.height), (1, 1));
        let archive = result.archive_mhtml.as_ref().expect("archive retained");
        assert_eq!(archive.mime, "multipart/related");
        assert!(archive.filename.ends_with(".mhtml"));
        assert_eq!(
            offline_cache_archive_bytes(archive).unwrap().len(),
            archive.bytes
        );
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].resource, "script");
        assert!(result.resources[0].allowed);
        assert_eq!(result.resources[1].resource, "image");
        assert!(!result.resources[1].allowed);
        let pdf = result.pdf_snapshot.as_ref().expect("PDF snapshot retained");
        assert_eq!(pdf.mime, "application/pdf");
        assert_eq!(pdf.filename, "mde-browser-123-example.test.pdf");
        assert_eq!(offline_cache_pdf_bytes(pdf).unwrap(), pdf_bytes);
        assert_eq!(result.text.chars().count(), OFFLINE_CACHE_TEXT_MAX_CHARS);
        assert!(!result.text.ends_with("tail"));

        let bad_source = body.replace("browser_offline_cache", "cloud_cache");
        assert!(parse_offline_cache_result(&bad_source).is_err());
        let bad_privacy = body.replace("offline_or_mesh_only", "public");
        assert!(parse_offline_cache_result(&bad_privacy).is_err());
        let bad_engine = body.replace(r#""engine":"cef""#, r#""engine":"webkit""#);
        assert!(parse_offline_cache_result(&bad_engine).is_err());
        let empty = body.replace(
            &format!(
                r#""text":"{}tail""#,
                "x".repeat(OFFLINE_CACHE_TEXT_MAX_CHARS)
            ),
            r#""text":"   ""#,
        );
        assert!(parse_offline_cache_result(&empty).is_err());
        let bad_archive_name = body.replace(".mhtml", "../bad.mhtml");
        assert!(parse_offline_cache_result(&bad_archive_name).is_err());
        let bad_resource = body.replace(r#""resource":"script""#, r#""resource":"cookie""#);
        assert!(parse_offline_cache_result(&bad_resource).is_err());
        let bad_pdf = body.replace(
            &pdf_data,
            &base64::engine::general_purpose::STANDARD.encode(b"not a pdf"),
        );
        assert!(parse_offline_cache_result(&bad_pdf).is_err());
    }

    #[test]
    fn browser_offline_cache_viewport_texture_decodes_and_caches_png() {
        let mut image = egui::ColorImage::new([4, 3], egui::Color32::TRANSPARENT);
        for y in 0..3 {
            for x in 0..4 {
                image.pixels[y * 4 + x] =
                    egui::Color32::WHITE.gamma_multiply((y * 4 + x + 1) as f32 / 12.0);
            }
        }
        let viewport = offline_cache_viewport_image(&image).expect("small viewport encodes");
        let ctx = egui::Context::default();

        let first = offline_cache_viewport_texture(&ctx, "cache-texture", &viewport)
            .expect("viewport texture decodes");
        assert_eq!(first.size(), [4, 3]);
        let second = offline_cache_viewport_texture(&ctx, "cache-texture", &viewport)
            .expect("cached viewport texture is reused");
        assert_eq!(first.id(), second.id());

        let mut mismatched = viewport.clone();
        mismatched.width = 5;
        assert!(
            offline_cache_viewport_texture(&ctx, "cache-texture-mismatch", &mismatched).is_none(),
            "decoded PNG dimensions must match the advertised viewport metadata"
        );
    }

    #[test]
    fn browser_offline_cache_archive_saves_valid_mhtml_to_disk() {
        let request = OfflineCacheRequest {
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://archive.example/".to_owned(),
            title: "Archive".to_owned(),
            viewport: None,
            resources: Vec::new(),
            pdf_snapshot: None,
        };
        let archive =
            offline_cache_mhtml_archive(&request, "Archived text", 123).expect("archive encodes");
        let mut state = WebState::default();
        state.latest_offline_cache = Some(BrowserOfflineCacheResult {
            host: local_hostname(),
            cache_id: "cache-archive".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://archive.example/".to_owned(),
            title: "Archive".to_owned(),
            text: "Archived text".to_owned(),
            viewport: None,
            resources: Vec::new(),
            archive_mhtml: Some(archive.clone()),
            pdf_snapshot: None,
            cached_ms: Some(123),
        });
        let dir = tempfile::tempdir().expect("temp archive dir");

        let path = state
            .save_latest_offline_cache_archive_to_dir(dir.path())
            .expect("archive saves");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(archive.filename.as_str())
        );
        let saved = std::fs::read(&path).expect("read saved archive");
        assert_eq!(saved.len(), archive.bytes);
        let saved = String::from_utf8(saved).expect("archive is utf8");
        assert!(saved.starts_with("MIME-Version: 1.0\r\n"));
        assert!(saved.contains("Archived text"));
    }

    #[test]
    fn browser_offline_cache_pdf_snapshot_saves_valid_pdf_to_disk() {
        let pdf_bytes = b"%PDF-1.7\n% cached PDF fixture\n";
        let pdf = OfflineCachePdf {
            mime: "application/pdf".to_owned(),
            filename: "mde-browser-123-archive.example.pdf".to_owned(),
            bytes: pdf_bytes.len(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(pdf_bytes),
        };
        let mut state = WebState::default();
        state.latest_offline_cache = Some(BrowserOfflineCacheResult {
            host: local_hostname(),
            cache_id: "cache-pdf".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://archive.example/".to_owned(),
            title: "Archive".to_owned(),
            text: "Archived text".to_owned(),
            viewport: None,
            resources: Vec::new(),
            archive_mhtml: None,
            pdf_snapshot: Some(pdf.clone()),
            cached_ms: Some(123),
        });
        let dir = tempfile::tempdir().expect("temp pdf dir");

        let path = state
            .save_latest_offline_cache_pdf_to_dir(dir.path())
            .expect("PDF saves");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(pdf.filename.as_str())
        );
        assert_eq!(std::fs::read(&path).expect("read saved PDF"), pdf_bytes);
    }

    #[test]
    fn browser_security_update_status_parser_surfaces_updater_posture() {
        let body = serde_json::json!({
            "node": local_hostname(),
            "state": "mismatch",
            "expected_cef_version": "149.0.6",
            "expected_chromium_version": "149.0.7827.201",
            "expected_channel": "stable",
            "active_runtime": "/opt/mde/cef",
            "installed_version": "old",
            "installed_chromium": "old",
            "libcef_present": true,
            "updater_state": "failed",
            "last_update_ms": 123,
            "last_update_exit_code": 69,
            "last_update_error": "installer unavailable",
            "last_error": "active CEF runtime does not match packaged manifest",
            "updated_ms": 124,
        })
        .to_string();

        let status = parse_security_update_status(&body).expect("valid security status");

        assert_eq!(status.node, local_hostname());
        assert_eq!(status.state, "mismatch");
        assert_eq!(
            status.expected_chromium_version.as_deref(),
            Some("149.0.7827.201")
        );
        assert_eq!(status.installed_version.as_deref(), Some("old"));
        assert!(status.libcef_present);
        assert_eq!(status.updater_state, "failed");
        assert_eq!(status.last_update_exit_code, Some(69));
        assert!(status.is_actionable());
        assert!(parse_security_update_status(r#"{"node":"n","state":"pretend"}"#).is_err());
        assert!(parse_security_update_status(r#"{"state":"current"}"#).is_err());
    }

    #[test]
    fn browser_speech_status_parsers_surface_worker_posture() {
        let read_body = serde_json::json!({
            "node": local_hostname(),
            "last_request_id": "01HTTS",
            "last_host": local_hostname(),
            "last_url": "https://example.test/",
            "last_title": "Example",
            "state": "unavailable",
            "last_error": "piper voice model is not installed",
            "accepted": 2,
            "spoken": 1,
            "rejected": 0,
            "last_request_ms": 123,
            "updated_ms": 124,
        })
        .to_string();
        let read_status = parse_read_aloud_status(&read_body).expect("read-aloud status");
        assert_eq!(read_status.node, local_hostname());
        assert_eq!(read_status.state, "unavailable");
        assert_eq!(read_status.last_title.as_deref(), Some("Example"));
        assert_eq!(read_status.accepted, 2);
        assert!(read_status.is_visible());
        assert!(read_status.is_actionable());
        assert_eq!(read_status.chip_label(), "TTS unavailable");
        assert!(parse_read_aloud_status(r#"{"node":"n","state":"pretend"}"#).is_err());

        let voice_body = serde_json::json!({
            "node": local_hostname(),
            "last_request_id": "01HSTT",
            "last_host": local_hostname(),
            "last_url": "https://example.test/",
            "last_mode": "dictation",
            "state": "listening",
            "last_error": null,
            "accepted": 3,
            "transcribed": 2,
            "rejected": 1,
            "last_transcript_chars": 17,
            "last_request_ms": 223,
            "updated_ms": 224,
        })
        .to_string();
        let voice_status = parse_voice_command_status(&voice_body).expect("voice status");
        assert_eq!(voice_status.node, local_hostname());
        assert_eq!(voice_status.state, "listening");
        assert_eq!(voice_status.last_mode.as_deref(), Some("dictation"));
        assert_eq!(voice_status.last_transcript_chars, Some(17));
        assert!(voice_status.is_visible());
        assert!(voice_status.is_actionable());
        assert_eq!(voice_status.chip_label(), "Dictation listening");
        assert!(parse_voice_command_status(
            r#"{"node":"n","state":"listening","last_mode":"song"}"#
        )
        .is_err());
    }

    #[test]
    fn browser_passkey_status_parser_surfaces_ceremony_posture() {
        let body = serde_json::json!({
            "node": local_hostname(),
            "last_request_id": "01HPASSKEY",
            "last_host": local_hostname(),
            "last_ceremony": "create",
            "last_rp_id": "example.test",
            "state": "pending",
            "mirrored": true,
            "last_error": null,
            "accepted": 1,
            "rejected": 0,
            "last_pending_ms": 333,
            "hardware_state": "ready",
            "hardware_key_count": 1,
            "hardware_readable_count": 1,
            "hardware_ctaphid_state": "init_request_ready",
            "hardware_ctaphid_init_frame_count": 1,
            "hardware_probe_ms": 332,
            "updated_ms": 334,
        })
        .to_string();
        let status = parse_passkey_status(&body).expect("passkey status");
        assert_eq!(status.node, local_hostname());
        assert_eq!(status.state, "pending");
        assert_eq!(status.last_ceremony.as_deref(), Some("create"));
        assert_eq!(status.last_rp_id.as_deref(), Some("example.test"));
        assert!(status.mirrored);
        assert!(status.ceremony_is_visible());
        assert!(status.hardware_is_visible());
        assert_eq!(status.chip_label(), "Passkey pending");
        assert_eq!(status.tone(), ChipTone::Info);
        assert_eq!(status.hardware_state, "ready");
        assert_eq!(status.hardware_key_count, 1);
        assert_eq!(status.hardware_readable_count, 1);
        assert_eq!(status.hardware_chip_label(), "Security key ready");
        assert_eq!(status.hardware_tone(), ChipTone::Ok);
        assert!(status.ctaphid_is_visible());
        assert_eq!(status.hardware_ctaphid_state, "init_request_ready");
        assert_eq!(status.hardware_ctaphid_init_frame_count, 1);
        assert_eq!(status.ctaphid_chip_label(), "CTAP INIT framed");
        assert_eq!(status.ctaphid_tone(), ChipTone::Info);
        let body = body.replace(r#""state":"pending""#, r#""state":"asserted""#);
        let asserted = parse_passkey_status(&body).expect("asserted passkey status");
        assert_eq!(asserted.chip_label(), "Passkey asserted");
        assert_eq!(asserted.tone(), ChipTone::Ok);
        let old_body = serde_json::json!({
            "node": "n",
            "state": "idle",
            "hardware_state": "unknown",
        })
        .to_string();
        let old_status = parse_passkey_status(&old_body).expect("old passkey status");
        assert_eq!(old_status.hardware_ctaphid_state, "unknown");
        assert_eq!(old_status.hardware_ctaphid_init_frame_count, 0);
        assert!(!old_status.ctaphid_is_visible());
        assert!(parse_passkey_status(r#"{"node":"n","state":"signed"}"#).is_err());
        assert!(
            parse_passkey_status(r#"{"node":"n","state":"pending","last_ceremony":"delete"}"#)
                .is_err()
        );
        assert!(
            parse_passkey_status(r#"{"node":"n","state":"idle","hardware_state":"wedged"}"#)
                .is_err()
        );
        assert!(parse_passkey_status(
            r#"{"node":"n","state":"idle","hardware_ctaphid_state":"wedged"}"#
        )
        .is_err());
        assert!(parse_passkey_status(
            r#"{"node":"n","state":"idle","hardware_ctaphid_state":"init_request_ready"}"#
        )
        .is_err());
        assert!(
            parse_passkey_status(
                r#"{"node":"n","state":"idle","hardware_ctaphid_state":"unavailable","hardware_ctaphid_init_frame_count":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn browser_speech_statuses_are_displayed_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let host = local_hostname();
        let read_body = serde_json::json!({
            "node": host,
            "last_request_id": "01HTTS",
            "last_host": host,
            "last_url": "https://example.test/",
            "last_title": "Example",
            "state": "speaking",
            "last_error": null,
            "accepted": 1,
            "spoken": 0,
            "rejected": 0,
            "last_request_ms": 123,
            "updated_ms": 124,
        })
        .to_string();
        persist
            .write(
                &browser_read_aloud_status_topic(&host),
                Priority::Min,
                None,
                Some(&read_body),
            )
            .expect("write read-aloud status");
        let voice_body = serde_json::json!({
            "node": host,
            "last_request_id": "01HSTT",
            "last_host": host,
            "last_url": "https://example.test/",
            "last_mode": "command",
            "state": "unavailable",
            "last_error": "STT runtime is not configured",
            "accepted": 1,
            "transcribed": 0,
            "rejected": 0,
            "last_transcript_chars": null,
            "last_request_ms": 223,
            "updated_ms": 224,
        })
        .to_string();
        persist
            .write(
                &browser_voice_command_status_topic(&host),
                Priority::Min,
                None,
                Some(&voice_body),
            )
            .expect("write voice status");

        state.poll_speech_statuses();

        let read_status = state
            .latest_read_aloud_status
            .as_ref()
            .expect("read-aloud status");
        assert_eq!(read_status.state, "speaking");
        assert_eq!(read_status.chip_label(), "TTS speaking");
        let voice_status = state
            .latest_voice_command_status
            .as_ref()
            .expect("voice status");
        assert_eq!(voice_status.state, "unavailable");
        assert_eq!(
            voice_status.last_error.as_deref(),
            Some("STT runtime is not configured")
        );
        assert_eq!(voice_status.chip_label(), "Voice unavailable");
    }

    #[test]
    fn browser_passkey_status_is_displayed_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let host = local_hostname();
        let body = serde_json::json!({
            "node": host,
            "last_request_id": "01HPASSKEY",
            "last_host": host,
            "last_ceremony": "get",
            "last_rp_id": "login.example.test",
            "state": "error",
            "mirrored": false,
            "last_error": "rp_id must match the origin host or a parent domain",
            "accepted": 1,
            "rejected": 1,
            "last_pending_ms": 444,
            "hardware_state": "present_permission_denied",
            "hardware_key_count": 1,
            "hardware_readable_count": 0,
            "hardware_ctaphid_state": "unavailable",
            "hardware_ctaphid_init_frame_count": 0,
            "hardware_probe_ms": 443,
            "updated_ms": 445,
        })
        .to_string();
        persist
            .write(
                &browser_passkey_status_topic(&host),
                Priority::Min,
                None,
                Some(&body),
            )
            .expect("write passkey status");

        state.poll_passkey_status();

        let status = state
            .latest_passkey_status
            .as_ref()
            .expect("passkey status");
        assert_eq!(status.state, "error");
        assert_eq!(status.last_ceremony.as_deref(), Some("get"));
        assert_eq!(
            status.last_error.as_deref(),
            Some("rp_id must match the origin host or a parent domain")
        );
        assert_eq!(status.hardware_state, "present_permission_denied");
        assert_eq!(status.hardware_key_count, 1);
        assert_eq!(status.hardware_readable_count, 0);
        assert_eq!(status.hardware_ctaphid_state, "unavailable");
        assert_eq!(status.hardware_ctaphid_init_frame_count, 0);
        assert!(!status.ctaphid_is_visible());
        assert_eq!(status.chip_label(), "Passkey error");
        assert_eq!(status.tone(), ChipTone::Warn);
        assert_eq!(status.hardware_chip_label(), "Security key blocked");
        assert_eq!(status.hardware_tone(), ChipTone::Warn);
    }

    #[test]
    fn browser_passkey_body_adds_browser_metadata_to_helper_event() {
        let body = browser_passkey_body(
            BrowserEngine::Cef,
            r#"{
                "ceremony":"create",
                "origin":"https://login.example/register",
                "rp_id":"login.example",
                "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                "client_request_id":"mde-pk-test-1",
                "user_handle_b64url":"user_handle_123",
                "user_name":"MDE User",
                "timeout_ms":60000,
                "user_present":true
            }"#,
        )
        .expect("passkey body");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_passkey");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["ceremony"], "create");
        assert_eq!(v["rp_id"], "login.example");
        assert_eq!(v["client_request_id"], "mde-pk-test-1");
        assert_eq!(v["user_name"], "MDE User");
        assert_eq!(v["timeout_ms"], 60000);
        // security-2: the user-presence signal is forwarded to the daemon.
        assert_eq!(v["user_present"], true);

        // A helper event with no presence signal forwards user_present=false, so
        // the daemon will not set the UP bit.
        let no_presence = browser_passkey_body(
            BrowserEngine::Cef,
            r#"{
                "ceremony":"get",
                "origin":"https://login.example/",
                "rp_id":"login.example",
                "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456"
            }"#,
        )
        .expect("passkey body");
        let no_presence: serde_json::Value =
            serde_json::from_str(&no_presence).expect("valid JSON");
        assert_eq!(no_presence["user_present"], false);
        assert!(browser_passkey_body(
            BrowserEngine::Servo,
            r#"{"ceremony":"delete","origin":"https://login.example","rp_id":"login.example","challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456"}"#
        )
        .is_err());
    }

    #[test]
    fn browser_passkey_helper_event_publishes_daemon_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PasskeyRequest {
                body: r#"{
                    "ceremony":"get",
                    "origin":"https://login.example/auth",
                    "rp_id":"login.example",
                    "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                    "client_request_id":"mde-pk-test-2",
                    "allow_credentials":["credential_id_123456"],
                    "timeout_ms":45000
                }"#
                .to_owned(),
            },
        );
        run_until_texture(&mut state);

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: sent ceremony to daemon")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_PASSKEY, None)
            .expect("list passkey actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("passkey body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_passkey");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["ceremony"], "get");
        assert_eq!(v["origin"], "https://login.example/auth");
        assert_eq!(v["rp_id"], "login.example");
        assert_eq!(v["client_request_id"], "mde-pk-test-2");
        assert_eq!(v["allow_credentials"][0], "credential_id_123456");
        assert_eq!(
            state.pending_passkey_requests.get("mde-pk-test-2"),
            Some(&0usize)
        );
    }

    #[test]
    fn browser_passkey_daemon_completion_returns_to_helper_page() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PasskeyRequest {
                body: r#"{
                    "ceremony":"get",
                    "origin":"https://login.example/auth",
                    "rp_id":"login.example",
                    "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                    "client_request_id":"mde-pk-test-3",
                    "allow_credentials":["credential_id_123456"]
                }"#
                .to_owned(),
            },
        );
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let completion = serde_json::json!({
            "op": "browser_passkey_assertion",
            "source": "browser_passkeys",
            "node": local_hostname(),
            "request_id": "01HPASSKEY",
            "client_request_id": "mde-pk-test-3",
            "host": local_hostname(),
            "engine": "cef",
            "ceremony": "get",
            "origin": "https://login.example/auth",
            "rp_id": "login.example",
            "credential_id_b64url": "credential_id_123456",
            "user_handle_b64url": "user_handle_123",
            "authenticator_data_b64url": "auth_data_123456",
            "client_data_json_b64url": "client_data_123456",
            "signature_b64url": "signature_123456",
            "sign_count": 1,
            "updated_ms": 777,
        })
        .to_string();
        persist
            .write(
                &browser_passkey_event_topic(&local_hostname()),
                Priority::Default,
                None,
                Some(&completion),
            )
            .expect("write passkey completion");

        state.passkey_result_last_poll = None;
        state.poll_passkey_results();
        let controls = drain_control_messages(&helper);
        let Some(mde_web_preview_client::ControlMsg::CompletePasskey { body }) =
            controls.iter().find(|msg| {
                matches!(
                    msg,
                    mde_web_preview_client::ControlMsg::CompletePasskey { .. }
                )
            })
        else {
            panic!("expected CompletePasskey control, got {controls:?}");
        };
        let returned: serde_json::Value = serde_json::from_str(body).expect("completion JSON");
        assert_eq!(returned["client_request_id"], "mde-pk-test-3");
        assert_eq!(returned["op"], "browser_passkey_assertion");
        assert!(
            !state.pending_passkey_requests.contains_key("mde-pk-test-3"),
            "completion removes the pending route"
        );
    }

    #[test]
    fn browser_offline_cache_results_are_displayed_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_offline_cache_result_topic(&local_hostname());
        let body = serde_json::json!({
            "op": "browser_offline_cache_record",
            "source": "browser_offline_cache",
            "node": local_hostname(),
            "cache_id": "cache-456",
            "host": local_hostname(),
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "text": "Cached page text.",
            "text_chars": 17,
            "cached_ms": 123,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&body))
            .expect("write offline-cache result");

        state.poll_offline_cache_results();
        let latest = state
            .latest_offline_cache
            .as_ref()
            .expect("offline-cache result displayed");
        assert_eq!(latest.cache_id, "cache-456");
        assert_eq!(latest.text, "Cached page text.");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Offline cache ready: 17 characters")
        );

        state.latest_offline_cache = None;
        state.offline_cache_result_last_poll = None;
        state.poll_offline_cache_results();
        assert!(
            state.latest_offline_cache.is_none(),
            "cursor prevents replaying the same offline-cache record"
        );
    }

    #[test]
    fn browser_security_update_status_is_displayed_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_security_update_status_topic(&local_hostname());
        let body = serde_json::json!({
            "node": local_hostname(),
            "state": "current",
            "expected_cef_version": "149.0.6",
            "expected_chromium_version": "149.0.7827.201",
            "expected_channel": "stable",
            "active_runtime": "/opt/mde/cef",
            "installed_version": "149.0.6",
            "installed_chromium": "149.0.7827.201",
            "libcef_present": true,
            "updater_state": "attempted",
            "last_update_ms": 123,
            "last_update_exit_code": 0,
            "updated_ms": 124,
        })
        .to_string();
        persist
            .write(&topic, Priority::Min, None, Some(&body))
            .expect("write security status");

        state.poll_security_update_status();

        let status = state
            .latest_security_update
            .as_ref()
            .expect("security status");
        assert_eq!(status.state, "current");
        assert_eq!(status.updater_state, "attempted");
        assert_eq!(status.last_update_exit_code, Some(0));
        assert!(!status.is_actionable());
    }

    fn offline_cache_result(url: &str, text: &str) -> BrowserOfflineCacheResult {
        BrowserOfflineCacheResult {
            host: local_hostname(),
            cache_id: "cache-fallback".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Servo,
            url: url.to_owned(),
            title: "Cached Example".to_owned(),
            text: text.to_owned(),
            viewport: None,
            resources: Vec::new(),
            archive_mhtml: None,
            pdf_snapshot: None,
            cached_ms: Some(123),
        }
    }

    #[cfg(feature = "live-helper")]
    fn seed_gate_notice_for_test(state: &mut WebState) {
        state.gate_notice = Some("helper unavailable".to_owned());
    }

    #[cfg(not(feature = "live-helper"))]
    fn seed_gate_notice_for_test(_state: &mut WebState) {}

    #[test]
    fn browser_offline_cache_indexes_records_for_gated_page_fallback() {
        let mut state = WebState::default();
        state.address = "https://example.test/".to_owned();
        seed_gate_notice_for_test(&mut state);
        state.apply_offline_cache_result(offline_cache_result(
            "https://example.test/",
            "Cached fallback body.",
        ));

        let fallback = state
            .offline_cache_fallback_for_unavailable()
            .expect("matching offline fallback");
        assert_eq!(fallback.text, "Cached fallback body.");
        assert_eq!(
            state
                .cached_snapshot_for_url(" https://example.test/ ")
                .expect("trimmed cache URL lookup")
                .cache_id,
            "cache-fallback"
        );
        assert!(
            run_panel(&mut state),
            "gated Browser state renders cached fallback body"
        );
    }

    #[test]
    fn browser_offline_cache_matches_canonical_url_aliases() {
        let mut state = WebState::default();
        state.apply_offline_cache_result(offline_cache_result(
            "HTTPS://Example.Test:443/search?b=2&a=1#section",
            "Canonical cached fallback.",
        ));

        let fallback = state
            .cached_snapshot_for_url("https://example.test/search?a=1&b=2#other")
            .expect("query-order and fragment-insensitive cache URL lookup");
        assert_eq!(fallback.text, "Canonical cached fallback.");
        assert_eq!(
            state
                .cached_snapshot_for_url("https://EXAMPLE.TEST/search?b=2&a=1")
                .expect("host casing cache URL lookup")
                .cache_id,
            "cache-fallback"
        );
    }

    #[test]
    fn browser_offline_cache_url_canonicalizer_is_conservative() {
        assert_eq!(
            canonical_http_cache_url("HTTP://Example.Test:80"),
            Some("http://example.test/".to_owned())
        );
        assert_eq!(
            canonical_http_cache_url("https://example.test/path?z=9&a=1&z=8#top"),
            Some("https://example.test/path?a=1&z=8&z=9".to_owned())
        );
        assert_eq!(
            canonical_http_cache_url("ftp://example.test/path"),
            None,
            "non-HTTP schemes stay exact-only"
        );
        assert_eq!(
            canonical_http_cache_url("https://user@example.test/path"),
            None,
            "userinfo stays exact-only instead of broadening private URLs"
        );
    }

    #[test]
    fn browser_offline_cache_renders_for_matching_crashed_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        state.apply_offline_cache_result(offline_cache_result(
            "https://example.test/",
            "Cached crash fallback.",
        ));
        drop(helper);
        for _ in 0..20 {
            state.tabs[0].session.poll();
            if state.tabs[0].session.is_crashed() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        assert!(state.tabs[0].session.is_crashed(), "test tab crashed");
        let fallback = state
            .offline_cache_fallback_for_unavailable()
            .expect("crashed tab uses matching offline cache");
        assert_eq!(fallback.text, "Cached crash fallback.");
        assert!(
            run_panel(&mut state),
            "crashed Browser tab renders cached fallback body"
        );
        assert!(
            !state.respawn_requested,
            "rendering the offline fallback does not auto-respawn the crashed helper"
        );
    }

    #[test]
    fn browser_translation_result_parser_is_private_and_bounded() {
        let body = serde_json::json!({
            "op": "browser_translation",
            "source": "browser_translate",
            "node": local_hostname(),
            "request_id": "01HTRANSLATE",
            "host": local_hostname(),
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "source_lang": "auto",
            "target_lang": "es",
            "translation": format!("{}tail", "x".repeat(TRANSLATION_RESULT_MAX_CHARS)),
            "translation_chars": TRANSLATION_RESULT_MAX_CHARS + 4,
            "updated_ms": 123,
        })
        .to_string();
        let result = parse_translation_result(&body).expect("valid translation result");
        assert_eq!(result.host, local_hostname());
        assert_eq!(result.tab_index, 2);
        assert_eq!(result.engine, BrowserEngine::Cef);
        assert_eq!(result.source_lang, "auto");
        assert_eq!(result.target_lang, "es");
        assert_eq!(
            result.translation.chars().count(),
            TRANSLATION_RESULT_MAX_CHARS
        );
        assert!(!result.translation.ends_with("tail"));

        let bad_source = body.replace("browser_translate", "cloud_translate");
        assert!(parse_translation_result(&bad_source).is_err());
        let bad_engine = body.replace(r#""engine":"cef""#, r#""engine":"webkit""#);
        assert!(parse_translation_result(&bad_engine).is_err());
        let empty = body.replace(
            &format!(
                r#""translation":"{}tail""#,
                "x".repeat(TRANSLATION_RESULT_MAX_CHARS)
            ),
            r#""translation":"   ""#,
        );
        assert!(parse_translation_result(&empty).is_err());
    }

    #[test]
    fn browser_translation_results_are_displayed_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_translation_result_topic(&local_hostname());
        let body = serde_json::json!({
            "op": "browser_translation",
            "source": "browser_translate",
            "node": local_hostname(),
            "request_id": "01HTRANSLATE",
            "host": local_hostname(),
            "tab_index": 0,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "source_lang": "auto",
            "target_lang": "es",
            "translation": "Hola desde la pagina.",
            "translation_chars": 21,
            "updated_ms": 123,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&body))
            .expect("write translation result");

        state.poll_translation_results();
        let latest = state
            .latest_translation
            .as_ref()
            .expect("translation result displayed");
        assert_eq!(latest.translation, "Hola desde la pagina.");
        assert_eq!(latest.target_lang, "es");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Translation ready: 21 characters")
        );

        state.latest_translation = None;
        state.translation_result_last_poll = None;
        state.poll_translation_results();
        assert!(
            state.latest_translation.is_none(),
            "cursor prevents replaying the same translation"
        );
    }

    #[test]
    fn browser_voice_command_menu_publishes_stt_handoffs_with_tab_context() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.tabs[state.active].engine = BrowserEngine::Cef;
        state.tabs[state.active].page_focused = true;
        run_until_texture(&mut state);
        // Draft an address AFTER the surface settled (the per-frame engine
        // sync has folded the committed URL) — the handoff must carry the
        // operator's draft distinctly from the engine URL.
        state.address = "https://example.test/current".to_owned();
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::VoiceCommand);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::Dictate);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_VOICE_COMMAND, None)
            .expect("list browser voice command actions");
        assert_eq!(msgs.len(), 2);
        let modes: Vec<String> = msgs
            .iter()
            .map(|msg| {
                let body = msg.body.as_deref().expect("voice command body");
                let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
                assert_eq!(v["op"], "browser_voice_command");
                assert_eq!(v["source"], "browser");
                assert_eq!(v["engine"], "cef");
                assert_eq!(v["url"], "https://example.test/");
                assert_eq!(v["title"], "Example");
                assert_eq!(v["address"], "https://example.test/current");
                assert_eq!(v["focus"], "page");
                assert_eq!(v["max_transcript_chars"], 4096);
                v["mode"].as_str().expect("mode").to_owned()
            })
            .collect();
        assert_eq!(modes, ["command", "dictation"]);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Dictation: sent STT request")
        );
    }

    #[test]
    fn browser_voice_transcript_parser_and_command_classifier_are_conservative() {
        let body = serde_json::json!({
            "op": "browser_voice_transcript",
            "source": "browser_voice_command",
            "node": local_hostname(),
            "request_id": "01HVOICE",
            "host": local_hostname(),
            "mode": "command",
            "tab_index": 2,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "address": "https://example.test/",
            "focus": "chrome",
            "transcript": "Search this page for mesh policy.",
            "transcript_chars": 33,
            "updated_ms": 123,
        })
        .to_string();
        let result = parse_voice_transcript_result(&body).expect("valid voice result");
        assert_eq!(result.mode, VoiceCommandMode::Command);
        assert_eq!(result.tab_index, 2);
        assert_eq!(
            voice_command_action(&result.transcript),
            Some(BrowserVoiceAction::Find("mesh policy".to_owned()))
        );
        assert_eq!(
            voice_command_action("find in page status beacon"),
            Some(BrowserVoiceAction::Find("status beacon".to_owned()))
        );
        assert_eq!(
            voice_command_action("open a new tab"),
            Some(BrowserVoiceAction::NewTab)
        );
        assert_eq!(
            voice_command_action("please send my passwords"),
            None,
            "unsupported transcripts must not become browser actions"
        );
        let bad = body.replace("browser_voice_transcript", "browser_voice_action");
        assert!(parse_voice_transcript_result(&bad).is_err());
    }

    #[test]
    fn browser_voice_command_results_are_applied_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_voice_command_result_topic(&local_hostname());
        let body = serde_json::json!({
            "op": "browser_voice_transcript",
            "source": "browser_voice_command",
            "node": local_hostname(),
            "request_id": "01HVOICE",
            "host": local_hostname(),
            "mode": "command",
            "tab_index": 0,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "address": "https://example.test/",
            "focus": "chrome",
            "transcript": "new tab",
            "transcript_chars": 7,
            "updated_ms": 123,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&body))
            .expect("write result");

        state.poll_voice_command_results();
        assert!(
            matches!(
                state.take_open_request(),
                Some(TabOpenIntent::NewForeground(_))
            ),
            "voice command result should enqueue one foreground tab"
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Voice command: new tab")
        );

        state.voice_command_result_last_poll = None;
        state.poll_voice_command_results();
        assert!(
            state.take_open_request().is_none(),
            "cursor prevents replaying the same transcript"
        );
    }

    #[test]
    fn browser_voice_dictation_result_inserts_text_only_when_page_is_focused() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.tabs[state.active].page_focused = true;
        let _ = drain_control_messages(&helper);

        state.apply_voice_transcript_result(VoiceTranscriptResult {
            host: local_hostname(),
            mode: VoiceCommandMode::Dictation,
            tab_index: 0,
            focus: "page".to_owned(),
            transcript: "hello mesh".to_owned(),
        });

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Text(text)
                ) if text == "hello mesh"
            )),
            "page-focused dictation should be forwarded as committed text: {controls:?}"
        );

        state.tabs[state.active].page_focused = false;
        state.apply_voice_transcript_result(VoiceTranscriptResult {
            host: local_hostname(),
            mode: VoiceCommandMode::Dictation,
            tab_index: 0,
            focus: "page".to_owned(),
            transcript: "ignored".to_owned(),
        });
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().all(|msg| !matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Text(_)
                )
            )),
            "dictation without page focus must not type into the helper"
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Dictation result ready: focus the page before dictating")
        );
    }

    #[test]
    fn browser_session_sync_publishes_once_until_the_snapshot_changes() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.publish_session_snapshot();
        state.publish_session_snapshot();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_SESSION_SYNC, None)
            .expect("list browser session sync");
        assert_eq!(msgs.len(), 1, "unchanged snapshots are de-duped");

        state.set_vertical_tabs(true);
        state.publish_session_snapshot();
        let msgs = persist
            .list_since(ACTION_BROWSER_SESSION_SYNC, None)
            .expect("list browser session sync after change");
        assert_eq!(msgs.len(), 2, "a changed setting emits a new snapshot");
        let latest: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("sync body")).expect("valid JSON");
        assert_eq!(latest["settings"]["vertical_tabs"], true);
    }

    #[test]
    fn browser_capture_success_publishes_notify_feed_event() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let path = PathBuf::from("/tmp/mde-browser-capture.png");

        state.record_capture_success("Captured", &path);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_NOTIFY_BROWSER, None)
            .expect("list browser notify events");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("notify body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["severity"], "info");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["summary"], "Captured /tmp/mde-browser-capture.png");
        assert_eq!(v["detail"], "/tmp/mde-browser-capture.png");
        assert_eq!(v["action"], "action/shell/goto/browser");
    }

    #[test]
    fn suggestions_only_fetch_for_plain_search_drafts() {
        assert!(should_fetch_suggestions("mesh browser"));
        assert!(!should_fetch_suggestions("https://example.com"));
        assert!(!should_fetch_suggestions("example.com"));
        assert!(!should_fetch_suggestions("localhost:8080"));
        assert!(!should_fetch_suggestions("about:blank"));
        assert!(!should_fetch_suggestions("   "));
        assert_eq!(
            suggestions_url("a+b & c"),
            "https://search.mesh/autocompleter?q=a%2Bb+%26+c"
        );
    }

    #[test]
    fn suggestions_parser_accepts_searxng_and_opensearch_shapes() {
        assert_eq!(
            parse_suggestions_json("mesh", r#"["mesh",["mesh network","mesh browser"]]"#)
                .expect("opensearch shape"),
            ["mesh network".to_owned(), "mesh browser".to_owned()]
        );
        assert_eq!(
            parse_suggestions_json(
                "mesh",
                r#"{"suggestions":["mesh browser",{"value":"mesh terminal"},{"text":"mesh files"}]}"#
            )
            .expect("object shape"),
            [
                "mesh browser".to_owned(),
                "mesh terminal".to_owned(),
                "mesh files".to_owned()
            ]
        );
        assert_eq!(
            parse_suggestions_json("mesh", r#"["mesh","mesh browser","mesh browser",""]"#)
                .expect("dedupe"),
            ["mesh browser".to_owned()]
        );
    }

    #[test]
    fn accepting_a_suggestion_uses_the_normal_omnibox_load_path() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        state.accept_suggestion("mesh browser".to_owned());

        assert_eq!(state.address, "https://search.mesh/search?q=mesh+browser");
        assert!(
            wait_for_fresh_frame(&mut state),
            "accepted suggestion reached the helper through submit_address"
        );
    }

    #[test]
    fn history_matches_feed_the_omnibox_suggestions_even_for_url_like_drafts() {
        let mut state = WebState::default();
        state
            .history
            .record("https://example.com/mesh-docs", "Mesh Docs", 1);
        state.history.record("https://other.test/", "Other", 2);

        // A URL-like draft skips the SearXNG fetch gate entirely (no thread is
        // spawned) — the history match must still surface independently of it.
        state.address = "https://example.com/mesh".to_owned();
        assert!(!should_fetch_suggestions(&state.address));
        state.update_suggestions_for_address();
        assert_eq!(
            state.suggestions.history,
            ["https://example.com/mesh-docs".to_owned()]
        );

        // An empty (or whitespace-only) draft shows no history matches at all,
        // even though the store still holds visits.
        state.address = "   ".to_owned();
        state.update_suggestions_for_address();
        assert!(state.suggestions.history.is_empty());
    }

    #[test]
    fn accepting_a_history_hit_uses_the_normal_omnibox_load_path() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        // History suggestions are plain visit-URL strings, flowing through the
        // exact same accept path as a search suggestion.
        state.accept_suggestion("https://example.com/visited".to_owned());

        assert_eq!(state.address, "https://example.com/visited");
        assert!(
            wait_for_fresh_frame(&mut state),
            "accepted history suggestion reached the helper through submit_address"
        );
    }

    #[test]
    fn dedup_search_items_omits_entries_already_shown_as_history_hits() {
        let items = vec![
            "https://example.com/mesh".to_owned(),
            "mesh browser".to_owned(),
        ];
        let history = vec!["https://example.com/mesh".to_owned()];

        let deduped: Vec<String> = dedup_search_items(&items, &history)
            .into_iter()
            .cloned()
            .collect();

        assert_eq!(deduped, vec!["mesh browser".to_owned()]);
    }

    #[test]
    fn next_selection_wraps_and_seeds_from_none() {
        // From nothing highlighted: Down picks the first, Up the last.
        assert_eq!(next_selection(None, 1, 3), Some(0));
        assert_eq!(next_selection(None, -1, 3), Some(2));
        // Wrap at both ends and step in the middle.
        assert_eq!(next_selection(Some(2), 1, 3), Some(0));
        assert_eq!(next_selection(Some(0), -1, 3), Some(2));
        assert_eq!(next_selection(Some(1), 1, 3), Some(2));
        // An empty list highlights nothing.
        assert_eq!(next_selection(Some(0), 1, 0), None);
        assert_eq!(next_selection(None, 1, 0), None);
    }

    #[test]
    fn inline_top_hit_preselects_only_a_genuine_completion() {
        let list = vec![
            "https://example.com/".to_string(),
            "https://other.com/".to_string(),
        ];
        // Draft is a prefix of the top hit → preselect it (case-insensitive).
        assert_eq!(inline_top_hit(&list, "https://exa"), Some(0));
        assert_eq!(inline_top_hit(&list, "HTTPS://EXA"), Some(0));
        // Empty draft → nothing.
        assert_eq!(inline_top_hit(&list, "  "), None);
        // Draft equals the top (nothing left to complete) → nothing.
        assert_eq!(inline_top_hit(&list, "https://example.com/"), None);
        // Draft is not a prefix of the top → nothing (arrows still work).
        assert_eq!(inline_top_hit(&list, "other"), None);
        // Empty list → nothing.
        assert_eq!(inline_top_hit(&[], "http"), None);
    }

    #[test]
    fn keyword_search_target_routes_configured_shortcuts() {
        let engines = default_search_engines();
        // "img sunset" → the mesh image-category search.
        assert_eq!(
            keyword_search_target("img sunset", &engines),
            Some("https://search.mesh/search?categories=images&q=sunset".to_owned())
        );
        // Case-insensitive keyword; the query is percent-encoded (space → '+').
        assert_eq!(
            keyword_search_target("VID a b", &engines),
            Some("https://search.mesh/search?categories=videos&q=a+b".to_owned())
        );
        // An unknown leading word is NOT a keyword → None (default router handles it).
        assert_eq!(keyword_search_target("cat videos", &engines), None);
        // A bare keyword with no query → None.
        assert_eq!(keyword_search_target("img", &engines), None);
        assert_eq!(keyword_search_target("img   ", &engines), None);
    }

    #[test]
    fn tab_group_color_cycles_over_the_palette() {
        // Distinct colors for successive groups, wrapping at the palette length (5).
        assert_ne!(tab_group_color(0), tab_group_color(1));
        assert_eq!(tab_group_color(0), tab_group_color(5));
        assert_eq!(tab_group_color(1), tab_group_color(6));
    }

    #[test]
    fn new_group_from_tab_assigns_then_ungroup_detaches() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(state.tabs[0].group.is_none());

        state.new_group_from_tab(0);
        assert_eq!(state.tabs[0].group, Some(0));
        assert_eq!(state.tab_groups.len(), 1);
        assert_eq!(state.tab_groups[0].color, tab_group_color(0));

        state.ungroup_tab(0);
        assert!(state.tabs[0].group.is_none());
        // The group itself remains so existing indices stay stable.
        assert_eq!(state.tab_groups.len(), 1);
    }

    #[test]
    fn suggestion_selection_commits_the_highlighted_value_in_render_order() {
        let mut s = SuggestionState::default();
        s.set_bookmark_matches(vec![BookmarkBarLink {
            title: "BM".into(),
            url: "https://bm.example/".into(),
        }]);
        s.set_history_matches(vec!["https://hist.example/".into()]);
        s.items = vec!["search term".into()];
        // Render order: bookmark, history, deduped search.
        assert_eq!(
            s.ordered_commit_values(),
            vec![
                "https://bm.example/".to_string(),
                "https://hist.example/".to_string(),
                "search term".to_string(),
            ]
        );
        // Nothing highlighted → Enter submits the typed draft (no committed value).
        assert!(s.selected_value().is_none());
        // Arrow down walks the list; the committed value follows the highlight.
        s.move_selection(1);
        assert_eq!(s.selected_value().as_deref(), Some("https://bm.example/"));
        s.move_selection(1);
        assert_eq!(s.selected_value().as_deref(), Some("https://hist.example/"));
        s.move_selection(1); // -> search
        s.move_selection(1); // wraps back to the first
        assert_eq!(s.selected_value().as_deref(), Some("https://bm.example/"));
    }

    #[test]
    fn thumbnail_size_preserves_aspect_and_caps_width() {
        // Wider than the cap → scaled down, aspect preserved (240 * 800/1280 = 150).
        let s = thumbnail_size(egui::vec2(1280.0, 800.0), 240.0);
        assert!((s.x - 240.0).abs() < 0.01);
        assert!((s.y - 150.0).abs() < 0.01);
        // Already narrower than the cap → not upscaled.
        let s2 = thumbnail_size(egui::vec2(100.0, 50.0), 240.0);
        assert!((s2.x - 100.0).abs() < 0.01 && (s2.y - 50.0).abs() < 0.01);
        // A degenerate frame yields no thumbnail.
        assert_eq!(
            thumbnail_size(egui::vec2(0.0, 800.0), 240.0),
            egui::Vec2::ZERO
        );
    }

    #[derive(Clone, Default)]
    struct RecordingTransfers {
        jobs: std::sync::Arc<std::sync::Mutex<Vec<TransferJob>>>,
        verbs: std::sync::Arc<std::sync::Mutex<Vec<TransferVerb>>>,
    }

    impl RecordingTransfers {
        fn with_jobs(jobs: Vec<TransferJob>) -> Self {
            Self {
                jobs: std::sync::Arc::new(std::sync::Mutex::new(jobs)),
                verbs: std::sync::Arc::default(),
            }
        }

        fn verbs(&self) -> Vec<TransferVerb> {
            self.verbs.lock().unwrap().clone()
        }

        fn set_jobs(&self, jobs: Vec<TransferJob>) {
            *self.jobs.lock().unwrap() = jobs;
        }
    }

    impl TransfersClient for RecordingTransfers {
        fn jobs(&self) -> Vec<TransferJob> {
            self.jobs.lock().unwrap().clone()
        }

        fn worker_present(&self) -> bool {
            true
        }

        fn dispatch(&self, verb: &TransferVerb) -> Result<(), String> {
            self.verbs.lock().unwrap().push(verb.clone());
            Ok(())
        }
    }

    #[test]
    fn browser_download_enqueue_submits_a_verified_browser_transfer() {
        let transfers = RecordingTransfers::default();
        let id = enqueue_browser_output(&transfers, "/tmp/helper/file.bin", "/home/mm/Downloads")
            .expect("enqueue");
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1);
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected submit");
        };
        assert_eq!(job.id, id);
        assert_eq!(job.source, "/tmp/helper/file.bin");
        assert_eq!(job.dest, "/home/mm/Downloads");
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert!(job.policy.verify, "browser outputs should be verified");
    }

    #[test]
    fn scraper_output_batch_enqueues_one_transfer_per_file() {
        let transfers = RecordingTransfers::default();
        let ids = enqueue_browser_output_batch(
            &transfers,
            &[
                "/tmp/scrape/page.json".to_owned(),
                "/tmp/scrape/page.md".to_owned(),
            ],
            "/home/mm/Exports",
        )
        .expect("enqueue batch");
        assert_eq!(ids.len(), 2);
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 2);
        for verb in verbs.iter() {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, "/home/mm/Exports");
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn active_page_scrape_export_writes_formats_and_queues_transfers() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        state.tabs[state.active].engine = BrowserEngine::Cef;
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 501,
                url: "https://example.test/products/page-2.html".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 502,
                url: "https://example.test/assets/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 503,
                url: "https://cdn.example.test/ignored.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        run_panel(&mut state);
        let spool = tempfile::tempdir().expect("scrape spool dir");
        let dest = tempfile::tempdir().expect("scrape destination dir");

        let ids = state
            .export_active_page_metadata_scrape_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("export active page scrape");

        assert_eq!(ids.len(), 3);
        let mut files = std::fs::read_dir(spool.path())
            .expect("read scrape spool")
            .map(|entry| entry.expect("scrape file").path())
            .collect::<Vec<_>>();
        files.sort();
        let exts = files
            .iter()
            .map(|path| path.extension().and_then(|ext| ext.to_str()).unwrap_or(""))
            .collect::<Vec<_>>();
        assert_eq!(exts, ["csv", "json", "md"]);
        let json = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "json"))
                .expect("json export"),
        )
        .expect("read json export");
        let v: serde_json::Value = serde_json::from_str(&json).expect("scrape JSON");
        assert_eq!(v["op"], "browser_active_page_scrape");
        assert_eq!(v["engine"], "cef");
        assert_eq!(
            v["scope"],
            "active_page_metadata_with_crawl_seed_text_and_dom"
        );
        assert_eq!(v["extracted_text_status"], "not_requested");
        assert_eq!(v["dom_extract_status"], "not_requested");
        assert_eq!(v["dom_link_count"], 0);
        assert_eq!(v["dom_heading_count"], 0);
        assert_eq!(v["crawl_seed_count"], 2);
        assert_eq!(v["crawl_manifest_status"], "ready");
        assert_eq!(v["crawl_execution_status"], "not_started");
        assert_eq!(v["crawl_manifest_max_depth"], 1);
        assert_eq!(v["crawl_manifest_count"], 2);
        let seed = v["crawl_seed"].as_array().expect("crawl seed array");
        assert!(seed
            .iter()
            .any(|item| item["url"] == "https://example.test/products/page-2.html"));
        assert!(seed
            .iter()
            .any(|item| item["url"] == "https://example.test/assets/app.js"));
        assert!(
            !seed
                .iter()
                .any(|item| item["url"] == "https://cdn.example.test/ignored.js"),
            "cross-origin telemetry must not become a crawl seed"
        );
        let crawl_manifest = v["crawl_manifest"].as_array().expect("crawl manifest");
        assert!(crawl_manifest.iter().any(|item| {
            item["url"] == "https://example.test/products/page-2.html"
                && item["source"] == "telemetry"
                && item["depth"] == 1
        }));
        assert!(
            !crawl_manifest
                .iter()
                .any(|item| item["url"] == "https://cdn.example.test/ignored.js"),
            "cross-origin telemetry must not become a crawl manifest target"
        );
        let csv = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "csv"))
                .expect("csv export"),
        )
        .expect("read csv export");
        assert!(
            csv.contains("captured_ms,engine,title,url,scope,seed_url,seed_resource,seed_allowed,text_status,text_chars,text_truncated,text,dom_kind,dom_url,dom_text,dom_level,dom_same_origin,dom_rel,dom_target")
        );
        assert!(csv.contains("\"Example\""));
        assert!(csv.contains("\"not_requested\""));
        assert!(csv.contains("\"https://example.test/products/page-2.html\""));
        assert!(csv.contains("crawl_manifest"));
        assert!(csv.contains("crawl_target"));
        assert!(!csv.contains("cdn.example.test"));
        let md = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "md"))
                .expect("markdown export"),
        )
        .expect("read markdown export");
        assert!(md.contains("# Example"));
        assert!(md.contains(
            "active page metadata with bounded crawl seed, extracted text, DOM links/headings"
        ));
        assert!(md.contains("Visible page text was not requested"));
        assert!(md.contains("DOM links were not requested"));
        assert!(md.contains("## Crawl Manifest"));
        assert!(md.contains("source=telemetry"));
        assert!(md.contains("https://example.test/assets/app.js"));

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 3);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn power_scrape_export_requests_page_scrape_and_writes_dom_extract() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 601,
                url: "https://example.test/article/related.html".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
            },
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("scrape spool dir");
        let dest = tempfile::tempdir().expect("scrape destination dir");

        state
            .request_active_page_metadata_scrape_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("request page DOM scrape export");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Scrape export: reading page DOM")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageScrape {
                    id: 1,
                    max_bytes: 65536,
                    max_links: 64,
                    max_headings: 32,
                }
            )),
            "scrape export must request bounded page DOM from the helper: {controls:?}"
        );
        assert_eq!(
            std::fs::read_dir(spool.path())
                .expect("read empty scrape spool")
                .count(),
            0,
            "scrape files wait for page DOM"
        );

        state.handle_page_scrape_event(
            1,
            serde_json::json!({
                "text": "  Visible article body.\n\nSecond paragraph.  ",
                "text_truncated": false,
                "article_text": "  Article lead.\n\nArticle detail.  ",
                "article_text_truncated": false,
                "article_selector": "article",
                "canonical_url": "https://example.test/article/",
                "meta_description": "An example article summary.",
                "document_lang": "en-US",
                "links": [
                    {
                        "url": "https://example.test/article/related.html",
                        "text": "Related Article",
                        "rel": "next",
                        "target": "_self"
                    },
                    {
                        "url": "https://example.test/article/part-2.html",
                        "text": "Part Two",
                        "rel": "",
                        "target": ""
                    },
                    {
                        "url": "https://elsewhere.test/",
                        "text": "External",
                        "rel": "",
                        "target": "_blank"
                    }
                ],
                "headings": [
                    {"level": 1, "text": "Story Headline"},
                    {"level": 2, "text": "Second Section"}
                ]
            })
            .to_string(),
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Power mode: queued active-page scrape export (3 files)")
        );

        let mut files = std::fs::read_dir(spool.path())
            .expect("read scrape spool")
            .map(|entry| entry.expect("scrape file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 3);
        let json = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "json"))
                .expect("json export"),
        )
        .expect("read json export");
        let v: serde_json::Value = serde_json::from_str(&json).expect("scrape JSON");
        assert_eq!(v["extracted_text_status"], "captured");
        assert_eq!(
            v["extracted_text"],
            "Visible article body.\n\nSecond paragraph."
        );
        assert_eq!(v["extracted_text_truncated"], false);
        assert_eq!(v["dom_extract_status"], "captured");
        assert_eq!(v["article_extract_status"], "captured");
        assert_eq!(v["article_text"], "Article lead.\n\nArticle detail.");
        assert_eq!(v["article_text_chars"], 30);
        assert_eq!(v["article_text_truncated"], false);
        assert_eq!(v["article_selector"], "article");
        assert_eq!(v["canonical_url"], "https://example.test/article/");
        assert_eq!(v["meta_description"], "An example article summary.");
        assert_eq!(v["document_lang"], "en-US");
        assert_eq!(v["dom_link_count"], 3);
        assert_eq!(v["dom_heading_count"], 2);
        assert_eq!(v["crawl_seed_count"], 1);
        assert_eq!(v["crawl_manifest_status"], "ready");
        assert_eq!(v["crawl_execution_status"], "not_started");
        assert_eq!(v["crawl_manifest_count"], 2);
        let links = v["dom_links"].as_array().expect("dom links");
        assert!(links.iter().any(|link| {
            link["url"] == "https://example.test/article/related.html"
                && link["text"] == "Related Article"
                && link["same_origin"] == true
        }));
        assert!(links.iter().any(|link| {
            link["url"] == "https://elsewhere.test/" && link["same_origin"] == false
        }));
        let crawl_manifest = v["crawl_manifest"].as_array().expect("crawl manifest");
        assert!(crawl_manifest.iter().any(|target| {
            target["url"] == "https://example.test/article/related.html"
                && target["source"] == "telemetry"
        }));
        assert!(crawl_manifest.iter().any(|target| {
            target["url"] == "https://example.test/article/part-2.html"
                && target["source"] == "dom_link"
        }));
        assert!(
            !crawl_manifest
                .iter()
                .any(|target| target["url"] == "https://elsewhere.test/"),
            "cross-origin DOM links stay out of the crawl manifest"
        );
        let csv = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "csv"))
                .expect("csv export"),
        )
        .expect("read csv export");
        assert!(csv.contains("\"captured\""));
        assert!(csv.contains("\"Visible article body."));
        assert!(csv.contains("dom_link"));
        assert!(csv.contains("\"Related Article\""));
        assert!(csv.contains("dom_heading"));
        assert!(csv.contains("\"Story Headline\""));
        assert!(csv.contains("dom_article"));
        assert!(csv.contains("\"Article lead."));
        assert!(csv.contains("dom_canonical"));
        assert!(csv.contains("\"https://example.test/article/\""));
        assert!(csv.contains("crawl_manifest"));
        assert!(csv.contains("\"https://example.test/article/part-2.html\""));
        assert!(csv.contains("dom_meta_description"));
        assert!(csv.contains("\"An example article summary.\""));
        assert!(csv.contains("dom_document_lang"));
        assert!(csv.contains("\"en-US\""));
        let md = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "md"))
                .expect("markdown export"),
        )
        .expect("read markdown export");
        assert!(md.contains("## Extracted Text"));
        assert!(md.contains("Visible article body."));
        assert!(md.contains("## Article Extract"));
        assert!(md.contains("Article lead."));
        assert!(md.contains("https://example.test/article/"));
        assert!(md.contains("An example article summary."));
        assert!(md.contains("## Crawl Manifest"));
        assert!(md.contains("source=dom_link"));
        assert!(md.contains("## DOM Links"));
        assert!(md.contains("[Related Article](https://example.test/article/related.html)"));
        assert!(md.contains("## DOM Headings"));
        assert!(md.contains("h1 Story Headline"));

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 3);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn media_manifest_export_sniffs_media_requests_and_queues_transfer() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            90,
            "https://cdn.example.test/app.js",
            mde_web_preview_client::ResourceType::Script,
        );
        resource(
            91,
            "https://cdn.example.test/poster.webp?cache=1",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            92,
            "https://video.example.test/master.m3u8",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            93,
            "https://video.example.test/manifest.mpd",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            94,
            "https://video.example.test/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("media spool dir");
        let dest = tempfile::tempdir().expect("media destination dir");

        let id = state
            .export_active_media_manifest_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("export media manifest");

        let mut files = std::fs::read_dir(spool.path())
            .expect("read media spool")
            .map(|entry| entry.expect("media file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].extension().and_then(|ext| ext.to_str()),
            Some("json")
        );
        let body = std::fs::read_to_string(&files[0]).expect("read media manifest");
        let v: serde_json::Value = serde_json::from_str(&body).expect("media manifest JSON");
        assert_eq!(v["op"], "browser_media_manifest");
        assert_eq!(v["scope"], "active_page_media_sniffer");
        assert_eq!(v["item_count"], 4);
        let items = v["items"].as_array().expect("items array");
        assert!(items.iter().any(|item| item["kind"] == "image"));
        assert!(items.iter().any(|item| item["kind"] == "hls"));
        assert!(items.iter().any(|item| item["kind"] == "dash"));
        assert!(items.iter().any(|item| item["kind"] == "video"));
        assert!(
            !items.iter().any(|item| item["url"]
                .as_str()
                .is_some_and(|url| url.ends_with("app.js"))),
            "non-media script requests stay out of the media manifest"
        );

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1);
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected submit");
        };
        assert_eq!(job.id, id);
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
        assert_eq!(job.source, files[0].to_string_lossy().as_ref());
        assert!(job.policy.verify);
    }

    #[test]
    fn media_asset_request_marks_blocked_resources_for_ignore_blocking() {
        let recent = vec![
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: true,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://video.example.test/master.m3u8".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
                allowed: false,
            },
        ];

        let requests = active_page_media_asset_requests(
            "https://example.test/watch",
            "Example Video",
            BrowserEngine::Cef,
            42,
            &recent,
        )
        .expect("encode media asset requests");

        assert_eq!(requests.len(), 1, "non-media script requests stay out");
        let v: serde_json::Value =
            serde_json::from_slice(&requests[0]).expect("media request JSON");
        assert_eq!(v["op"], "browser_media_download_request");
        assert_eq!(v["asset_url"], "https://video.example.test/master.m3u8");
        assert_eq!(v["kind"], "hls");
        assert_eq!(v["allowed_by_page_filter"], false);
        assert_eq!(v["ignore_blocking"], true);
        assert_eq!(v["suggested_filename"], "master.m3u8");
        assert_eq!(v["rename_strategy"], "auto_rename_by_url_hint");
    }

    #[test]
    fn media_asset_request_selection_batches_only_observed_images() {
        let recent = vec![
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: true,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://cdn.example.test/hero.png".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: true,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://cdn.example.test/photo".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: false,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://video.example.test/clip.mp4".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Media,
                ),
                allowed: true,
            },
        ];

        let requests = active_page_media_asset_requests_with_selection(
            "https://example.test/gallery",
            "Example Gallery",
            BrowserEngine::Cef,
            42,
            &recent,
            MediaAssetSelection::Images,
        )
        .expect("encode image asset requests");

        assert_eq!(requests.len(), 2);
        let bodies = requests
            .iter()
            .map(|body| serde_json::from_slice::<serde_json::Value>(body).expect("request JSON"))
            .collect::<Vec<_>>();
        assert!(bodies.iter().all(|v| v["kind"] == "image"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/hero.png"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/photo"));
        assert!(bodies.iter().any(|v| v["ignore_blocking"] == true));
        assert!(!bodies
            .iter()
            .any(|v| v["asset_url"] == "https://video.example.test/clip.mp4"));
    }

    #[test]
    fn observed_media_download_queue_writes_request_files_and_transfers() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            90,
            "https://cdn.example.test/app.js",
            mde_web_preview_client::ResourceType::Script,
        );
        resource(
            91,
            "https://cdn.example.test/poster.webp?cache=1",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            92,
            "https://video.example.test/master.m3u8",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            93,
            "https://video.example.test/manifest.mpd",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            94,
            "https://video.example.test/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("media download spool dir");
        let dest = tempfile::tempdir().expect("media download destination dir");

        let ids = state
            .download_observed_media_assets_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("queue observed media downloads");

        assert_eq!(ids.len(), 4);
        let mut files = std::fs::read_dir(spool.path())
            .expect("read media download spool")
            .map(|entry| entry.expect("media request file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 4);
        let bodies = files
            .iter()
            .map(|path| {
                let body = std::fs::read_to_string(path).expect("read request file");
                serde_json::from_str::<serde_json::Value>(&body).expect("request JSON")
            })
            .collect::<Vec<_>>();
        assert!(bodies
            .iter()
            .all(|v| v["op"] == "browser_media_download_request"));
        assert!(bodies.iter().any(|v| v["kind"] == "image"));
        assert!(bodies.iter().any(|v| v["kind"] == "hls"));
        assert!(bodies.iter().any(|v| v["kind"] == "dash"));
        assert!(bodies.iter().any(|v| v["kind"] == "video"));

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 4);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.source.ends_with(".download.json"));
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn intercepted_download_becomes_a_browser_download_ledger_job() {
        // B2: a CEF `on_before_download` interception (surfaced by the helper as an
        // EventMsg::Download and drained in the pump) is handed to the Transfers
        // ledger — NOT saved locally. Prove the ledger job + its `.download.json`
        // manifest carry the asset URL the daemon will fetch.
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(7, "https://files.example.test/report.pdf", "report.pdf");

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1, "exactly one ledger submission");
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert!(job.source.ends_with(".download.json"));
        assert_eq!(job.dest, browser_capture_dir().to_string_lossy().as_ref());

        let body = std::fs::read_to_string(&job.source).expect("manifest written to spool");
        let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest JSON");
        assert_eq!(manifest["op"], "browser_media_download_request");
        assert_eq!(
            manifest["asset_url"],
            "https://files.example.test/report.pdf"
        );
        assert_eq!(manifest["suggested_filename"], "report.pdf");
        // The interception opens the Downloads drawer so the user sees it land.
        assert!(state.downloads_open);
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn intercepted_download_without_a_filename_derives_one_from_the_url() {
        // A `Content-Disposition`-less download arrives with an empty suggested
        // name; the last non-empty URL segment becomes the filename.
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(
            9,
            "https://dl.example.test/a/b/archive.tar.gz?token=x",
            "",
        );

        let verbs = transfers.verbs();
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        let body = std::fs::read_to_string(&job.source).expect("manifest written to spool");
        let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest JSON");
        assert_eq!(manifest["suggested_filename"], "archive.tar.gz");
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn download_is_dangerous_flags_executable_and_script_extensions() {
        for filename in [
            "setup.exe",
            "Invoice.pdf.exe",
            "script.PS1",
            "x.jar",
            "installer.MSI",
            "payload.scr",
            "run.bat",
            "run.cmd",
            "legacy.com",
            "auto.pif",
            "app.msix",
            "creds.vbs",
            "creds.vbe",
            "worker.js",
            "worker.jse",
            "task.wsf",
            "help.hta",
            "panel.cpl",
            "lib.dll",
            "shortcut.lnk",
            "tweak.reg",
            "install.sh",
            "binary.run",
            "package.deb",
            "package.rpm",
            "image.dmg",
            "bundle.pkg",
            "app.apk",
            "widget.gadget",
            // A masquerading double extension from either side.
            "notes.exe.pdf",
        ] {
            assert!(
                download_is_dangerous(filename),
                "{filename} should be flagged dangerous"
            );
        }
    }

    #[test]
    fn download_is_dangerous_allows_ordinary_files() {
        for filename in [
            "photo.jpg",
            "report.pdf",
            "archive.tar.gz",
            "data.csv",
            "notes.txt",
            "song.mp3",
            "video.mp4",
            ".bashrc",
            "README",
            "",
        ] {
            assert!(
                !download_is_dangerous(filename),
                "{filename} should NOT be flagged dangerous"
            );
        }
    }

    #[test]
    fn dangerous_download_parks_pending_and_does_not_submit() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(11, "https://files.example.test/setup.exe", "setup.exe");

        assert!(
            transfers.verbs().is_empty(),
            "a dangerous download must not touch the ledger before Keep/Discard"
        );
        let pending = state
            .pending_dangerous_download
            .clone()
            .expect("dangerous download parked pending confirmation");
        assert_eq!(pending.id, 11);
        assert_eq!(pending.url, "https://files.example.test/setup.exe");
        assert_eq!(pending.filename, "setup.exe");
        // The drawer opens so the user actually sees the warning.
        assert!(state.downloads_open);
    }

    #[test]
    fn keeping_a_dangerous_download_submits_exactly_one_ledger_job() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(12, "https://files.example.test/setup.exe", "setup.exe");
        assert!(state.pending_dangerous_download.is_some());

        state.keep_pending_dangerous_download();

        assert!(
            state.pending_dangerous_download.is_none(),
            "Keep resolves the pending confirmation"
        );
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1, "exactly one ledger submission on Keep");
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        let body = std::fs::read_to_string(&job.source).expect("manifest written to spool");
        let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest JSON");
        assert_eq!(
            manifest["asset_url"],
            "https://files.example.test/setup.exe"
        );
        assert_eq!(manifest["suggested_filename"], "setup.exe");
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn discarding_a_dangerous_download_drops_it_with_no_ledger_job() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(13, "https://files.example.test/setup.exe", "setup.exe");
        assert!(state.pending_dangerous_download.is_some());

        state.discard_pending_dangerous_download();

        assert!(state.pending_dangerous_download.is_none());
        assert!(
            transfers.verbs().is_empty(),
            "Discard must never create a ledger job"
        );
    }

    #[test]
    fn safe_download_submits_immediately_without_parking() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(14, "https://files.example.test/report.pdf", "report.pdf");

        assert!(
            state.pending_dangerous_download.is_none(),
            "a safe download never parks for confirmation"
        );
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1, "exactly one ledger submission, no parking");
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn drawer_remove_hides_a_job_and_clear_all_hides_every_job() {
        // `RecordingTransfers::jobs()` is a static double for the daemon-owned
        // ledger — it never shrinks on its own — so this proves dismissal is a
        // Browser-local view filter, NOT a mutation of the shared ledger.
        let a = transfer_fixture(
            "browser-a",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            10,
        );
        let b = transfer_fixture(
            "browser-b",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            20,
        );
        let transfers = RecordingTransfers::with_jobs(vec![a, b]);
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        assert_eq!(state.download_jobs.len(), 2);

        state.dismiss_download("browser-a");
        assert_eq!(state.download_jobs.len(), 1);
        assert!(!state.download_jobs.iter().any(|job| job.id == "browser-a"));
        // The ledger itself still carries both jobs — only the Browser's view
        // dropped one — and a dismissed id stays hidden across a rebuild.
        assert_eq!(transfers.jobs().len(), 2);
        state.refresh_downloads();
        assert_eq!(state.download_jobs.len(), 1);
        assert_eq!(state.download_jobs[0].id, "browser-b");

        state.dismiss_all_downloads();
        assert!(state.download_jobs.is_empty());
        state.refresh_downloads();
        assert!(
            state.download_jobs.is_empty(),
            "Clear all stays hidden across a ledger refresh too"
        );
        assert_eq!(
            transfers.jobs().len(),
            2,
            "Clear all never mutates the shared ledger"
        );
    }

    #[test]
    fn copy_link_source_url_is_tracked_per_ledger_job() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(17, "https://files.example.test/report.pdf", "report.pdf");

        let verbs = transfers.verbs();
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        assert_eq!(
            state.download_source_urls.get(&job.id).map(String::as_str),
            Some("https://files.example.test/report.pdf")
        );
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn observed_image_download_queue_writes_only_image_request_files_and_transfers() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            90,
            "https://cdn.example.test/app.js",
            mde_web_preview_client::ResourceType::Script,
        );
        resource(
            91,
            "https://cdn.example.test/hero.png",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            92,
            "https://cdn.example.test/photo",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            93,
            "https://video.example.test/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("image download spool dir");
        let dest = tempfile::tempdir().expect("image download destination dir");

        let ids = state
            .download_observed_image_assets_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("queue observed image downloads");

        assert_eq!(ids.len(), 2);
        let mut files = std::fs::read_dir(spool.path())
            .expect("read image download spool")
            .map(|entry| entry.expect("image request file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 2);
        let bodies = files
            .iter()
            .map(|path| {
                let body = std::fs::read_to_string(path).expect("read request file");
                serde_json::from_str::<serde_json::Value>(&body).expect("request JSON")
            })
            .collect::<Vec<_>>();
        assert!(bodies.iter().all(|v| v["kind"] == "image"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/hero.png"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/photo"));
        assert!(!bodies
            .iter()
            .any(|v| v["asset_url"] == "https://video.example.test/clip.mp4"));

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 2);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.source.ends_with(".download.json"));
            assert!(job.policy.verify);
        }
    }

    fn transfer_fixture(
        id: &str,
        method: TransferMethod,
        state: TransferState,
        updated_ms: u64,
    ) -> TransferJob {
        let mut job = TransferJob::new(
            format!("/tmp/{id}.bin"),
            "/home/mm/Downloads",
            method,
            TransferPolicy {
                bwlimit: None,
                verify: true,
            },
        );
        job.id = id.to_owned();
        job.state = state;
        job.progress = if state == TransferState::Running {
            Some(42)
        } else {
            None
        };
        job.created_ms = updated_ms.saturating_sub(10);
        job.updated_ms = updated_ms;
        job
    }

    #[test]
    fn browser_download_manager_filters_and_dispatches_shared_transfer_jobs() {
        let running = transfer_fixture(
            "browser-running",
            TransferMethod::BrowserDownload,
            TransferState::Running,
            30,
        );
        let paused = transfer_fixture(
            "browser-paused",
            TransferMethod::BrowserDownload,
            TransferState::Paused,
            40,
        );
        let done = transfer_fixture(
            "browser-done",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            50,
        );
        let http = transfer_fixture("http", TransferMethod::Http, TransferState::Running, 60);
        let transfers = RecordingTransfers::with_jobs(vec![done, http, paused, running]);
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));

        let ids = state
            .download_jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["browser-running", "browser-paused", "browser-done"]);
        assert_eq!(state.download_counts(), (2, 3));

        state.dispatch_download_verb(TransferVerb::Pause("browser-running".to_owned()));
        state.dispatch_download_verb(TransferVerb::Resume("browser-paused".to_owned()));
        state.dispatch_download_verb(TransferVerb::Cancel("browser-done".to_owned()));

        assert_eq!(
            transfers.verbs(),
            [
                TransferVerb::Pause("browser-running".to_owned()),
                TransferVerb::Resume("browser-paused".to_owned()),
                TransferVerb::Cancel("browser-done".to_owned())
            ]
        );
    }

    #[test]
    fn browser_download_completion_publishes_notify_feed_event_once() {
        let bus = tempfile::tempdir().expect("temp bus");
        let running = transfer_fixture(
            "browser-running",
            TransferMethod::BrowserDownload,
            TransferState::Running,
            10,
        );
        let done = transfer_fixture(
            "browser-running",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            20,
        );
        let transfers = RecordingTransfers::with_jobs(vec![running]);
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));

        transfers.set_jobs(vec![done]);
        state.refresh_downloads();
        state.refresh_downloads();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_NOTIFY_BROWSER, None)
            .expect("list browser notify events");
        assert_eq!(msgs.len(), 1, "download completion is announced once");
        let body = msgs[0].body.as_deref().expect("notify body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["severity"], "info");
        assert_eq!(v["source"], "browser");
        assert_eq!(
            v["summary"],
            "Browser download complete: browser-running.bin"
        );
        assert_eq!(
            v["detail"],
            "/tmp/browser-running.bin -> /home/mm/Downloads"
        );
        assert_eq!(v["action"], "action/shell/goto/browser");
    }

    #[test]
    fn the_live_page_url_and_title_feed_the_page_actions() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        // The three page-actions all read the active session's live nav + title
        // (the testkit helper reports `about:blank` for both).
        let tab = &state.tabs[0];
        assert_eq!(tab.session.nav().url, "about:blank");
        assert_eq!(tab.session.title(), "about:blank");
        // The chrome now carrying the page-actions star menu still renders.
        assert!(
            run_panel(&mut state),
            "the page-actions chrome produced no draw"
        );
    }

    // ── live-helper: the real spawn/attach/pump glue ────────────────────────────
    //
    // Exercised through the SAME `open_with` seam the shell's Browser arm drives,
    // but with a `testkit` factory injected in place of the real `Command::spawn`,
    // so the spawn→attach→pump path runs hermetically — no Servo, no real process.

    #[cfg(feature = "live-helper")]
    #[test]
    fn live_open_spawns_attaches_and_pumps_a_frame() {
        use std::cell::RefCell;
        // Hold the fake helpers so the attached session stays live through the pump.
        let helpers: RefCell<Vec<testkit::FakeHelper>> = RefCell::new(Vec::new());
        let mut state = WebState::default();
        // A real, existing path passes the "helper installed" gate; the injected
        // factory returns a testkit session instead of exec'ing Servo.
        let bin = std::env::current_exe().expect("test exe path");
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            bin,
            |_spec| {
                let (session, helper) = testkit::connect()?;
                helpers.borrow_mut().push(helper);
                Ok(session)
            },
        );
        assert_eq!(
            state.tabs.len(),
            1,
            "the live open attached exactly one tab"
        );
        assert!(
            state.gate_notice.is_none(),
            "a successful open clears the gate notice"
        );
        assert!(
            run_until_texture(&mut state),
            "the live tab pumped no frame into the texture path"
        );
        assert!(state.tabs[0].texture.is_some());
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn live_open_with_no_seat_stays_the_honest_empty_state() {
        use std::cell::Cell;
        let spawned = Cell::new(false);
        let mut state = WebState::default();
        let bin = std::env::current_exe().expect("test exe path");
        state.open_with(
            false,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            bin,
            |_spec| {
                spawned.set(true);
                Err(std::io::Error::other(
                    "factory must not be called without a seat",
                ))
            },
        );
        assert!(!spawned.get(), "no seat must not spawn a helper");
        assert!(state.tabs.is_empty(), "no tab attaches without a seat");
        assert!(
            state.gate_notice.is_some(),
            "the no-seat gate is named honestly"
        );
        // The panel draws the honest gated EmptyState, never a fake page.
        assert!(run_panel(&mut state));
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn live_open_with_an_absent_helper_stays_the_honest_empty_state() {
        use std::cell::Cell;
        let spawned = Cell::new(false);
        let mut state = WebState::default();
        let missing = std::path::PathBuf::from("/nonexistent/mde-web-preview-xyz");
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            missing,
            |_spec| {
                spawned.set(true);
                Err(std::io::Error::other(
                    "factory must not run with an absent helper",
                ))
            },
        );
        assert!(!spawned.get(), "an absent helper binary must not spawn");
        assert!(state.tabs.is_empty());
        let notice = state.gate_notice.as_deref().unwrap_or_default();
        assert!(
            notice.contains("not installed"),
            "the absent-helper gate names it honestly: {notice}"
        );
        assert!(run_panel(&mut state));
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn a_spawn_failure_surfaces_an_honest_reason_not_a_hang() {
        let mut state = WebState::default();
        let bin = std::env::current_exe().expect("test exe path");
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            bin,
            |_spec| Err(std::io::Error::other("exec denied by sandbox")),
        );
        assert!(state.tabs.is_empty(), "a failed spawn attaches no tab");
        let notice = state.gate_notice.as_deref().unwrap_or_default();
        assert!(
            notice.contains("failed to start") && notice.contains("exec denied"),
            "a spawn failure surfaces its reason: {notice}"
        );
        assert!(run_panel(&mut state), "the honest failure notice draws");
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn helper_bin_path_defaults_and_honors_engine_env_overrides() {
        use std::path::PathBuf;
        std::env::remove_var(SERVO_HELPER_BIN_ENV);
        std::env::remove_var(CEF_HELPER_BIN_ENV);
        assert_eq!(
            helper_bin_path(BrowserEngine::Servo),
            PathBuf::from(DEFAULT_SERVO_HELPER_BIN)
        );
        assert_eq!(
            helper_bin_path(BrowserEngine::Cef),
            PathBuf::from(DEFAULT_CEF_HELPER_BIN)
        );
        std::env::set_var(SERVO_HELPER_BIN_ENV, "/opt/mde/mde-web-preview");
        std::env::set_var(CEF_HELPER_BIN_ENV, "/opt/mde/mde-web-cef");
        assert_eq!(
            helper_bin_path(BrowserEngine::Servo),
            PathBuf::from("/opt/mde/mde-web-preview")
        );
        assert_eq!(
            helper_bin_path(BrowserEngine::Cef),
            PathBuf::from("/opt/mde/mde-web-cef")
        );
        std::env::remove_var(SERVO_HELPER_BIN_ENV);
        std::env::remove_var(CEF_HELPER_BIN_ENV);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn default_engine_prefers_cef_only_when_helper_and_runtime_are_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let helper = dir.path().join("mde-web-cef");
        let runtime = dir.path().join("cef");
        std::fs::write(&helper, b"helper").expect("helper marker");
        std::fs::create_dir_all(runtime.join(CEF_RELEASE_DIR)).expect("release dir");
        std::fs::create_dir_all(runtime.join(CEF_RESOURCES_DIR)).expect("resources dir");
        std::fs::write(runtime.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"cef")
            .expect("libcef marker");
        std::fs::write(runtime.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu")
            .expect("icu marker");
        std::fs::write(
            runtime.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK),
            b"pak",
        )
        .expect("pak marker");

        std::env::set_var(CEF_HELPER_BIN_ENV, &helper);
        std::env::set_var(CEF_ROOT_ENV, &runtime);
        assert_eq!(
            WebState::default().engine,
            BrowserEngine::Cef,
            "a Workstation with the packaged CEF helper/runtime should default to Chromium"
        );

        std::fs::remove_file(runtime.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK))
            .expect("remove resources marker");
        assert_eq!(
            WebState::default().engine,
            BrowserEngine::Servo,
            "a partial CEF runtime must fall back to Servo instead of selecting a broken default"
        );
        std::env::remove_var(CEF_HELPER_BIN_ENV);
        std::env::remove_var(CEF_ROOT_ENV);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_open_requires_the_real_cef_runtime_before_spawn() {
        use std::cell::Cell;
        let spawned = Cell::new(false);
        let mut state = WebState::default();
        let bin = std::env::current_exe().expect("test exe path");
        std::env::remove_var(CEF_ROOT_ENV);
        state.open_with(
            true,
            BrowserEngine::Cef,
            START_URL.to_owned(),
            bin,
            |_spec| {
                spawned.set(true);
                Err(std::io::Error::other(
                    "factory must not run without the CEF runtime",
                ))
            },
        );
        assert!(!spawned.get(), "missing CEF runtime must gate before spawn");
        assert!(state.tabs.is_empty());
        let notice = state.gate_notice.as_deref().unwrap_or_default();
        assert!(
            notice.contains("Chromium/CEF runtime") && notice.contains(CEF_LIB_NAME),
            "the CEF runtime gate names the missing library: {notice}"
        );
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_live_open_uses_the_browser_ui_spawn_path_and_pumps_a_frame() {
        use std::cell::RefCell;
        let dir = make_fake_cef_runtime("mde-shell-cef-open-test");
        std::env::set_var(CEF_ROOT_ENV, &dir);

        let helpers: RefCell<Vec<testkit::FakeHelper>> = RefCell::new(Vec::new());
        let mut state = WebState::default();
        state.select_engine(BrowserEngine::Cef);
        let bin = std::env::current_exe().expect("test exe path");
        let expected_bin = bin.clone();
        state.open_with(
            true,
            BrowserEngine::Cef,
            START_URL.to_owned(),
            bin,
            |spec| {
                assert_eq!(spec.helper_bin, expected_bin);
                assert_eq!(spec.url, START_URL);
                assert_eq!((spec.width, spec.height), (INIT_W, INIT_H));
                let (session, helper) = testkit::connect()?;
                helpers.borrow_mut().push(helper);
                Ok(session)
            },
        );

        assert_eq!(state.tabs.len(), 1, "CEF live open attached one tab");
        assert_eq!(state.tabs[0].engine, BrowserEngine::Cef);
        assert!(
            state.gate_notice.is_none(),
            "successful CEF open clears the gate"
        );
        assert!(
            run_until_texture(&mut state),
            "CEF-selected Browser UI path did not pump a frame"
        );
        assert!(state.tabs[0].texture.is_some());

        std::env::remove_var(CEF_ROOT_ENV);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_live_browser_ui_renders_a_real_site_when_farm_smoke_is_enabled() {
        if std::env::var_os("MDE_CEF_LIVE_UI_SMOKE").is_none() {
            return;
        }

        let helper_bin = helper_bin_path(BrowserEngine::Cef);
        assert!(
            helper_bin.exists(),
            "MDE_WEB_CEF_BIN must point at a built mde-web-cef helper for the live smoke: {}",
            helper_bin.display()
        );
        assert_eq!(
            cef_runtime_missing_path(),
            None,
            "MDE_CEF_ROOT must point at a complete pinned CEF runtime"
        );

        let server = LiveHttpServer::start();
        let url = server.url.clone();
        let mut state = WebState::default();
        state.select_engine(BrowserEngine::Cef);
        state.open_with(
            true,
            BrowserEngine::Cef,
            START_URL.to_owned(),
            helper_bin,
            WebSession::spawn,
        );

        assert_eq!(state.tabs.len(), 1, "CEF live smoke attached one tab");
        assert_eq!(state.tabs[0].engine, BrowserEngine::Cef);
        assert!(
            run_until_texture(&mut state),
            "CEF did not produce the initial Browser UI frame"
        );

        state.tabs[0].texture = None;
        state.address = url.clone();
        state.submit_address();
        assert!(
            state.insecure_prompt.is_some(),
            "the live HTTP smoke should exercise the Browser HTTPS prompt seam"
        );
        state.continue_insecure_load();
        assert!(
            run_until_texture_for(&mut state, 300),
            "CEF did not render the live HTTP page through the Browser UI texture path"
        );
        assert!(
            server.hits() > 0,
            "CEF did not fetch the live smoke page at {url}"
        );

        if let Some(public_url) = std::env::var("MDE_CEF_LIVE_UI_PUBLIC_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
        {
            state.tabs[0].texture = None;
            state.address = public_url.clone();
            state.submit_address();
            assert!(
                state.insecure_prompt.is_none(),
                "public CEF smoke URLs must be HTTPS or otherwise pre-approved: {public_url}"
            );
            assert!(
                run_until_texture_for(&mut state, 600),
                "CEF did not render the public Browser UI smoke URL: {public_url}"
            );
        }
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_runtime_gate_accepts_the_upstream_bundle_layout() {
        let dir = make_fake_cef_runtime("mde-shell-cef-runtime-test");
        std::env::set_var(CEF_ROOT_ENV, &dir);
        assert_eq!(
            cef_runtime_lib(),
            dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME)
        );
        assert_eq!(cef_runtime_missing_path(), None);
        std::env::remove_var(CEF_ROOT_ENV);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(feature = "live-helper")]
    fn make_fake_cef_runtime(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(CEF_RELEASE_DIR)).expect("mkdir");
        std::fs::write(dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"test")
            .expect("libcef marker");
        std::fs::create_dir_all(dir.join(CEF_RESOURCES_DIR)).expect("resources");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu").expect("icu marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK), b"pak")
            .expect("pak marker");
        dir
    }

    #[cfg(feature = "live-helper")]
    struct LiveHttpServer {
        url: String,
        addr: std::net::SocketAddr,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(feature = "live-helper")]
    impl LiveHttpServer {
        fn start() -> Self {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::sync::Arc;
            use std::time::Duration;

            let listener = TcpListener::bind("127.0.0.1:0").expect("bind live smoke server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking live smoke server");
            let addr = listener.local_addr().expect("live smoke addr");
            let hits = Arc::new(AtomicUsize::new(0));
            let done = Arc::new(AtomicBool::new(false));
            let server_hits = Arc::clone(&hits);
            let server_done = Arc::clone(&done);
            let handle = std::thread::spawn(move || {
                let body = b"<!doctype html><html><body><h1>CEF Browser UI live smoke</h1><p>real HTTP page</p></body></html>";
                while !server_done.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buf = [0_u8; 1024];
                            let _ = stream.read(&mut buf);
                            server_hits.fetch_add(1, Ordering::SeqCst);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.write_all(body);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url: format!("http://{addr}/"),
                addr,
                hits,
                done,
                handle: Some(handle),
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[cfg(feature = "live-helper")]
    impl Drop for LiveHttpServer {
        fn drop(&mut self) {
            use std::net::TcpStream;
            use std::sync::atomic::Ordering;
            self.done.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    // ── BOOKMARKS-BAR ─────────────────────────────────────────────────────────
    /// Build a converged daemon [`mde_bookmarks::Collection`] fixture by minting
    /// real `AddBookmark` ops (top-level, in the given order) — the exact op the
    /// mackesd bookmarks worker replays, so the fold + serialize round-trip mirrors
    /// production, not a hand-forged JSON blob.
    fn fake_bookmark_collection(entries: &[(&str, &str)]) -> mde_bookmarks::Collection {
        let author = mde_bookmarks::Author::new("tester".into(), "test-node".into());
        let mut collection = mde_bookmarks::Collection::new();
        for (i, (title, url)) in entries.iter().enumerate() {
            collection.apply(&mde_bookmarks::Op::new(
                mde_bookmarks::Hlc::new(100 + i as u64, 0, "test-node".into()),
                author.clone(),
                mde_bookmarks::OpKind::AddBookmark {
                    id: uuid::Uuid::from_u128(0x1000 + i as u128),
                    parent: None,
                    order_key: format!("a{i}"),
                    url: (*url).to_string(),
                    title: (*title).to_string(),
                    favicon_ref: None,
                    tags: Vec::new(),
                    notes: String::new(),
                    added: 100,
                    source: mde_bookmarks::Source::Manual,
                },
            ));
        }
        collection
    }

    #[test]
    fn bookmark_bar_links_fold_top_level_bookmarks_in_render_order() {
        let mut collection = fake_bookmark_collection(&[
            ("Beta", "https://beta.example/"),
            ("", "https://blank-title.example/"),
        ]);
        // A top-level folder is NOT a bar button — the bar is a flat link strip.
        collection.apply(&mde_bookmarks::Op::new(
            mde_bookmarks::Hlc::new(200, 0, "test-node".into()),
            mde_bookmarks::Author::new("tester".into(), "test-node".into()),
            mde_bookmarks::OpKind::AddFolder {
                id: uuid::Uuid::from_u128(0x2000),
                name: "Work".to_string(),
                parent: None,
                order_key: "a9".to_string(),
            },
        ));

        let links = bookmark_bar_links_from(&collection);
        assert_eq!(links.len(), 2, "the folder is omitted from the bar");
        assert_eq!(links[0].title, "Beta");
        assert_eq!(links[0].url, "https://beta.example/");
        // A blank stored title falls back to the URL so the button stays legible.
        assert_eq!(links[1].title, "https://blank-title.example/");
    }

    #[test]
    fn all_bookmarked_urls_includes_nested_folder_bookmarks_and_normalizes_slash() {
        let mut collection = fake_bookmark_collection(&[("Top", "https://top.example/")]);
        let author = mde_bookmarks::Author::new("tester".into(), "test-node".into());
        let folder_id = uuid::Uuid::from_u128(0x3000);
        // A folder, and a bookmark nested INSIDE it (parent = folder id).
        collection.apply(&mde_bookmarks::Op::new(
            mde_bookmarks::Hlc::new(300, 0, "test-node".into()),
            author.clone(),
            mde_bookmarks::OpKind::AddFolder {
                id: folder_id,
                name: "Work".to_string(),
                parent: None,
                order_key: "b0".to_string(),
            },
        ));
        collection.apply(&mde_bookmarks::Op::new(
            mde_bookmarks::Hlc::new(301, 0, "test-node".into()),
            author,
            mde_bookmarks::OpKind::AddBookmark {
                id: uuid::Uuid::from_u128(0x3001),
                parent: Some(folder_id),
                order_key: "a0".to_string(),
                url: "https://nested.example/page".to_string(),
                title: "Nested".to_string(),
                favicon_ref: None,
                tags: Vec::new(),
                notes: String::new(),
                added: 100,
                source: mde_bookmarks::Source::Manual,
            },
        ));

        let all = all_bookmarks(&collection);
        assert_eq!(
            all.len(),
            2,
            "top-level AND nested folder bookmark both counted"
        );
        let urls = bookmarked_url_set(&all);
        // Trailing slash normalized on the stored side.
        assert!(urls.contains("https://top.example"));
        assert!(urls.contains("https://nested.example/page"));
        // Membership key lights the star whether the live page URL has the slash or not.
        assert!(urls.contains(bookmark_membership_key("https://top.example/")));
        assert!(urls.contains(bookmark_membership_key("https://top.example")));
        assert!(!urls.contains(bookmark_membership_key("https://unbookmarked.example/")));
    }

    #[test]
    fn matching_bookmarks_ranks_title_prefix_then_url_then_substring() {
        let index = vec![
            BookmarkBarLink {
                title: "Rust docs".into(),
                url: "https://doc.rust-lang.org/".into(),
            },
            BookmarkBarLink {
                title: "News".into(),
                url: "https://rust-news.example/".into(),
            },
            BookmarkBarLink {
                title: "Crates".into(),
                url: "https://crates.io/".into(),
            },
        ];
        // Empty draft → no suggestions (don't dump the whole set).
        assert!(matching_bookmarks(&index, "  ", 5).is_empty());
        // "rust" matches the first two; title-prefix ("Rust docs") ranks above the
        // url-substring match ("News" whose URL contains rust-news).
        let hits = matching_bookmarks(&index, "rust", 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust docs", "title-prefix ranks first");
        assert_eq!(hits[1].title, "News", "url-substring match ranked lower");
        // Cap is honored, and a no-match draft yields nothing.
        assert_eq!(matching_bookmarks(&index, "http", 1).len(), 1);
        assert!(matching_bookmarks(&index, "zzz", 5).is_empty());
    }

    #[test]
    fn bookmark_bar_visible_count_reserves_an_overflow_slot() {
        let (btn, gap, over) = (100.0, 2.0, 26.0);
        // Everything fits → no overflow slot, all shown.
        assert_eq!(bookmark_bar_visible_count(3, 400.0, btn, gap, over), 3);
        // Exactly the full-row width still shows them all.
        let exact = 3.0 * btn + 2.0 * gap;
        assert_eq!(bookmark_bar_visible_count(3, exact, btn, gap, over), 3);
        // Too narrow for all 4 → reserve the ">>" slot and show fewer (< total).
        let v = bookmark_bar_visible_count(4, exact, btn, gap, over);
        assert!(v < 4, "an overflow split shows fewer than the total");
        assert!(v >= 1, "a comfortable width still shows some buttons");
        // A sliver of width shows none — the whole set lives in the overflow menu.
        assert_eq!(bookmark_bar_visible_count(4, 20.0, btn, gap, over), 0);
        // Empty collection → nothing.
        assert_eq!(bookmark_bar_visible_count(0, 400.0, btn, gap, over), 0);
    }

    #[test]
    fn browser_bookmarks_bar_mirrors_the_collection_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let collection = fake_bookmark_collection(&[
            ("Example News", "https://news.example/"),
            ("Docs", "https://docs.example/"),
        ]);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        persist
            .write(
                STATE_BOOKMARKS_COLLECTION,
                Priority::Default,
                None,
                Some(&serde_json::to_string(&collection).expect("serialize collection")),
            )
            .expect("write collection");

        state.poll_bookmarks_collection();
        assert_eq!(
            state
                .bookmark_bar_links
                .iter()
                .map(|l| (l.title.as_str(), l.url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Example News", "https://news.example/"),
                ("Docs", "https://docs.example/"),
            ]
        );

        // The cursor prevents re-folding the same retained snapshot.
        state.bookmarks_collection_last_poll = None;
        state.poll_bookmarks_collection();
        assert_eq!(state.bookmark_bar_links.len(), 2, "no duplicate fold");

        // A newer converged snapshot replaces the row.
        let updated = fake_bookmark_collection(&[("Only One", "https://one.example/")]);
        persist
            .write(
                STATE_BOOKMARKS_COLLECTION,
                Priority::Default,
                None,
                Some(&serde_json::to_string(&updated).expect("serialize updated")),
            )
            .expect("write updated collection");
        state.bookmarks_collection_last_poll = None;
        state.poll_bookmarks_collection();
        assert_eq!(state.bookmark_bar_links.len(), 1);
        assert_eq!(state.bookmark_bar_links[0].url, "https://one.example/");
    }

    #[test]
    fn browser_bookmarks_bar_toggle_shows_and_hides_the_row() {
        let mut state = WebState::default();
        state.bookmark_bar_links = vec![BookmarkBarLink {
            title: "Example".to_owned(),
            url: "https://example.test/".to_owned(),
        }];
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);

        // Hidden by default (matching the other chrome toggles): no bar button.
        let out = run_panel_output(&ctx, &mut state, body_input());
        assert!(!state.bookmarks_bar_visible);
        assert!(
            !accesskit_nodes(&out)
                .iter()
                .any(|(_, n)| n.label() == Some("Example")),
            "a hidden bar renders no bookmark button"
        );

        // View → Show Bookmarks Bar reveals the button.
        state.toggle_bookmarks_bar();
        assert!(state.bookmarks_bar_visible);
        let out = run_panel_output(&ctx, &mut state, body_input());
        assert!(
            accesskit_nodes(&out)
                .iter()
                .any(|(_, n)| n.label() == Some("Example")),
            "a shown bar renders the bookmark button"
        );

        // Toggling again hides it.
        state.toggle_bookmarks_bar();
        assert!(!state.bookmarks_bar_visible);
    }

    #[test]
    fn browser_bookmarks_bar_overflow_menu_holds_the_extras() {
        // A narrow bar with more bookmarks than fit: the leading run shows on the
        // row and the rest live behind the ">>" menu. Assert the split via the pure
        // fit fn on the same fixed geometry the renderer uses.
        let total = 40usize;
        let narrow = 3.0 * BOOKMARK_BTN_W; // room for only a couple buttons
        let visible = bookmark_bar_visible_count(
            total,
            narrow,
            BOOKMARK_BTN_W,
            CHROME_GAP,
            BOOKMARK_OVERFLOW_W,
        );
        assert!(visible < total, "not all bookmarks fit the narrow row");
        assert!(visible >= 1, "at least one bookmark shows before the menu");
        assert!(
            total - visible >= 1,
            "the overflow menu holds the remaining bookmarks"
        );
    }

    #[test]
    fn browser_bookmark_click_navigates_active_tab_and_middle_click_opens_a_new_tab() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        // Plain click → navigate the active tab and sync the omnibox.
        state.open_bookmark("https://news.example/".to_owned(), false);
        assert_eq!(state.address, "https://news.example/");
        assert!(
            state.take_open_request().is_none(),
            "a plain click reuses the active tab, no new-tab intent"
        );

        // Middle click → open a new foreground tab on the preferred engine.
        state.open_bookmark("https://docs.example/".to_owned(), true);
        assert!(
            matches!(
                state.take_open_request(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://docs.example/"
            ),
            "a middle click enqueues a new foreground tab for the bookmark"
        );
    }

    #[test]
    fn browser_bookmark_click_with_no_open_tab_opens_a_new_tab() {
        let mut state = WebState::default();
        assert!(state.tabs.is_empty());
        state.open_bookmark("https://news.example/".to_owned(), false);
        assert!(
            matches!(
                state.take_open_request(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://news.example/"
            ),
            "with no active tab a click opens the bookmark in a new tab"
        );
    }
}

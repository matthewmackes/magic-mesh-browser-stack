//! `mde-web-wire` — the typed, length-prefixed message set that travels on the
//! per-session Unix socket: the socket half of the BOOKMARKS-6 browser seam.
//!
//! This is the SINGLE source of the wire contract, depended on by all three ends
//! that speak it — the shell client (`mde-web-preview-client`) and both
//! out-of-process engine helpers (`mde-web-preview` / Servo and `mde-web-cef` /
//! Chromium). Previously the one `wire.rs` source lived in the client crate and
//! was `#[path]`-included into each excluded helper, compiling the same file into
//! three crates with no shared type identity; extracting it here gives one crate,
//! one type identity, and no drift.
//!
//! Two directions, both framed identically on the wire:
//!
//! ```text
//!   [u32 LE payload-length][payload]
//! ```
//!
//! * [`ControlMsg`] — shell → helper: navigate / reload / history / resize / input.
//! * [`EventMsg`] — helper → shell: the shm fd is attached (`AttachFrame`, carrying
//!   the descriptor out-of-band via `SCM_RIGHTS`), a fresh frame was painted
//!   (`PaintReady`), the title / nav-state changed, or the page crashed.
//!
//! The payload is a compact, hand-rolled binary encoding (a `u8` tag then LE
//! fields) — no serde dependency, and every field is length- or bounds-checked on
//! decode so a malformed or hostile frame is a typed [`WireError`], never a panic
//! (§9). The key contract ([`KeyCode`]) is engine-neutral on purpose: the shell
//! maps egui keys onto it and the sandboxed helper maps it onto Servo, so neither
//! end leaks its toolkit's key numbering onto the wire.

use std::fmt;

/// A frame's declared payload length may not exceed this.
///
/// A guard against a corrupt/hostile length prefix allocating unboundedly.
/// Control/event payloads are tiny (a URL string at most); 1 MiB is enormous
/// headroom.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// Why a wire message could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The buffer ended before a field was fully read.
    UnexpectedEnd,
    /// A message/enum tag byte was not a known variant.
    BadTag(u8),
    /// A declared length exceeded [`MAX_FRAME_LEN`].
    TooLong(usize),
    /// A string field was not valid UTF-8.
    BadUtf8,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => f.write_str("wire message ended mid-field"),
            Self::BadTag(t) => write!(f, "unknown wire tag {t}"),
            Self::TooLong(n) => write!(f, "wire length {n} exceeds the cap"),
            Self::BadUtf8 => f.write_str("wire string was not valid UTF-8"),
        }
    }
}

impl std::error::Error for WireError {}

/// A pointer button, engine-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerButton {
    /// The primary (usually left) button.
    Primary = 0,
    /// The secondary (usually right) button.
    Secondary = 1,
    /// The middle button.
    Middle = 2,
}

impl PointerButton {
    const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Primary),
            1 => Some(Self::Secondary),
            2 => Some(Self::Middle),
            _ => None,
        }
    }
}

/// Engine-neutral cursor shape the page requested — the helper maps the engine's
/// native cursor type onto this small set, the shell maps it onto its UI cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CursorKind {
    /// The default arrow.
    #[default]
    Default = 0,
    /// A link / clickable (hand).
    Pointer = 1,
    /// Editable text (I-beam).
    Text = 2,
    /// A precise target (crosshair).
    Crosshair = 3,
    /// Busy / loading.
    Wait = 4,
    /// Progress (busy but interactive).
    Progress = 5,
    /// Contextual help.
    Help = 6,
    /// Move / all-scroll.
    Move = 7,
    /// Grab (draggable).
    Grab = 8,
    /// Grabbing (mid-drag).
    Grabbing = 9,
    /// Not allowed / no-drop.
    NotAllowed = 10,
    /// Horizontal resize (E/W, col).
    ResizeHorizontal = 11,
    /// Vertical resize (N/S, row).
    ResizeVertical = 12,
    /// Diagonal resize ↗↙ (NE/SW).
    ResizeNeSw = 13,
    /// Diagonal resize ↘↖ (NW/SE).
    ResizeNwSe = 14,
    /// Zoom in.
    ZoomIn = 15,
    /// Zoom out.
    ZoomOut = 16,
}

impl CursorKind {
    /// Decode from the wire byte, defaulting unknown values to [`Self::Default`].
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Pointer,
            2 => Self::Text,
            3 => Self::Crosshair,
            4 => Self::Wait,
            5 => Self::Progress,
            6 => Self::Help,
            7 => Self::Move,
            8 => Self::Grab,
            9 => Self::Grabbing,
            10 => Self::NotAllowed,
            11 => Self::ResizeHorizontal,
            12 => Self::ResizeVertical,
            13 => Self::ResizeNeSw,
            14 => Self::ResizeNwSe,
            15 => Self::ZoomIn,
            16 => Self::ZoomOut,
            _ => Self::Default,
        }
    }
}

/// Keyboard-modifier bitflags, engine-neutral (matches the common egui set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(pub u8);

impl Modifiers {
    /// `Ctrl` is held.
    pub const CTRL: u8 = 1 << 0;
    /// `Shift` is held.
    pub const SHIFT: u8 = 1 << 1;
    /// `Alt` is held.
    pub const ALT: u8 = 1 << 2;
    /// The platform command key (`Super`/`Cmd`) is held.
    pub const COMMAND: u8 = 1 << 3;

    /// Whether `flag` (one of the associated constants) is set.
    #[must_use]
    pub const fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

/// An engine-neutral key.
///
/// The shell maps its toolkit's `egui::Key` onto this and the helper maps it
/// onto Servo, so the wire never carries either toolkit's private key numbering.
/// Keys neither side has a mapping for are dropped (best-effort input) — printable
/// characters ride [`InputEvent::Text`] anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
#[allow(missing_docs, reason = "the variants are self-describing key names")]
pub enum KeyCode {
    Enter = 0,
    Escape,
    Backspace,
    Tab,
    Space,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl KeyCode {
    /// The wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Decode a wire discriminant back to a [`KeyCode`].
    #[must_use]
    pub const fn from_u16(v: u16) -> Option<Self> {
        // The variants are a dense 0..=63 range, so a bounds check + transmute
        // would be sound, but an explicit match keeps this `unsafe`-free.
        Some(match v {
            0 => Self::Enter,
            1 => Self::Escape,
            2 => Self::Backspace,
            3 => Self::Tab,
            4 => Self::Space,
            5 => Self::Delete,
            6 => Self::Insert,
            7 => Self::Home,
            8 => Self::End,
            9 => Self::PageUp,
            10 => Self::PageDown,
            11 => Self::ArrowUp,
            12 => Self::ArrowDown,
            13 => Self::ArrowLeft,
            14 => Self::ArrowRight,
            15 => Self::A,
            16 => Self::B,
            17 => Self::C,
            18 => Self::D,
            19 => Self::E,
            20 => Self::F,
            21 => Self::G,
            22 => Self::H,
            23 => Self::I,
            24 => Self::J,
            25 => Self::K,
            26 => Self::L,
            27 => Self::M,
            28 => Self::N,
            29 => Self::O,
            30 => Self::P,
            31 => Self::Q,
            32 => Self::R,
            33 => Self::S,
            34 => Self::T,
            35 => Self::U,
            36 => Self::V,
            37 => Self::W,
            38 => Self::X,
            39 => Self::Y,
            40 => Self::Z,
            41 => Self::Num0,
            42 => Self::Num1,
            43 => Self::Num2,
            44 => Self::Num3,
            45 => Self::Num4,
            46 => Self::Num5,
            47 => Self::Num6,
            48 => Self::Num7,
            49 => Self::Num8,
            50 => Self::Num9,
            51 => Self::F1,
            52 => Self::F2,
            53 => Self::F3,
            54 => Self::F4,
            55 => Self::F5,
            56 => Self::F6,
            57 => Self::F7,
            58 => Self::F8,
            59 => Self::F9,
            60 => Self::F10,
            61 => Self::F11,
            62 => Self::F12,
            _ => return None,
        })
    }
}

/// One forwarded input event, in the helper's **device pixels** (the shell has
/// already multiplied logical coordinates by `pixels_per_point`).
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// The pointer moved to `(x, y)` device pixels.
    PointerMoved {
        /// Device-pixel X.
        x: f32,
        /// Device-pixel Y.
        y: f32,
    },
    /// A pointer button changed state at `(x, y)` device pixels.
    PointerButton {
        /// Device-pixel X.
        x: f32,
        /// Device-pixel Y.
        y: f32,
        /// Which button.
        button: PointerButton,
        /// `true` on press, `false` on release.
        pressed: bool,
        /// Held keyboard modifiers at click time — carries Ctrl/Shift/Cmd-click
        /// (open-in-tab, extend-selection) through to the engine.
        modifiers: Modifiers,
    },
    /// The pointer left the view (so the helper can clear hover).
    PointerGone,
    /// A scroll/wheel delta in device pixels.
    Scroll {
        /// Horizontal delta.
        delta_x: f32,
        /// Vertical delta.
        delta_y: f32,
        /// Held keyboard modifiers — carries ctrl-wheel zoom + shift-wheel
        /// horizontal scroll through to the engine.
        modifiers: Modifiers,
    },
    /// A key changed state.
    Key {
        /// The engine-neutral key.
        key: KeyCode,
        /// `true` on press, `false` on release.
        pressed: bool,
        /// Held modifiers.
        modifiers: Modifiers,
    },
    /// Committed text (IME / typed characters).
    Text(String),
}

impl InputEvent {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::PointerMoved { x, y } => {
                out.push(0);
                put_f32(out, *x);
                put_f32(out, *y);
            }
            Self::PointerButton {
                x,
                y,
                button,
                pressed,
                modifiers,
            } => {
                out.push(1);
                put_f32(out, *x);
                put_f32(out, *y);
                out.push(*button as u8);
                out.push(u8::from(*pressed));
                out.push(modifiers.0);
            }
            Self::PointerGone => out.push(2),
            Self::Scroll {
                delta_x,
                delta_y,
                modifiers,
            } => {
                out.push(3);
                put_f32(out, *delta_x);
                put_f32(out, *delta_y);
                out.push(modifiers.0);
            }
            Self::Key {
                key,
                pressed,
                modifiers,
            } => {
                out.push(4);
                put_u16(out, key.as_u16());
                out.push(u8::from(*pressed));
                out.push(modifiers.0);
            }
            Self::Text(s) => {
                out.push(5);
                put_str(out, s);
            }
        }
    }

    fn decode(c: &mut Cursor<'_>) -> Result<Self, WireError> {
        Ok(match c.u8()? {
            0 => Self::PointerMoved {
                x: c.f32()?,
                y: c.f32()?,
            },
            1 => Self::PointerButton {
                x: c.f32()?,
                y: c.f32()?,
                button: PointerButton::from_u8(c.u8()?).ok_or(WireError::BadTag(0))?,
                pressed: c.bool()?,
                modifiers: Modifiers(c.u8()?),
            },
            2 => Self::PointerGone,
            3 => Self::Scroll {
                delta_x: c.f32()?,
                delta_y: c.f32()?,
                modifiers: Modifiers(c.u8()?),
            },
            4 => Self::Key {
                key: KeyCode::from_u16(c.u16()?).ok_or(WireError::BadTag(4))?,
                pressed: c.bool()?,
                modifiers: Modifiers(c.u8()?),
            },
            5 => Self::Text(c.string()?),
            t => return Err(WireError::BadTag(t)),
        })
    }
}

/// A message the shell sends to drive the helper.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMsg {
    /// Navigate to `url`.
    Load(String),
    /// Reload the current page.
    Reload,
    /// Stop the current page load, when the active helper exposes a real cancel
    /// hook.
    Stop,
    /// Go back one history entry.
    Back,
    /// Go forward one history entry.
    Forward,
    /// The view was resized to `width` x `height` **device** pixels.
    Resize {
        /// New width in device pixels.
        width: u32,
        /// New height in device pixels.
        height: u32,
    },
    /// Forward one input event (device pixels).
    Input(InputEvent),
    /// The shell's verdict for a helper resource-policy query
    /// ([`EventMsg::ResourceRequest`]) — BOOKMARKS-7 ad-filter. `allow = false`
    /// means the ad-filter engine matched a block rule, so the helper must drop
    /// the subresource **before** fetch.
    ResourceVerdict {
        /// Correlates with the [`EventMsg::ResourceRequest`] `id`.
        id: u64,
        /// `true` to fetch, `false` to drop (blocked before the network).
        allow: bool,
    },
    /// Push the page's cosmetic user-stylesheet — the element-hide selectors the
    /// helper injects into the rendered frame to hide leftover ad frames
    /// (BOOKMARKS-7). JS-off safe: it is a plain `display:none` stylesheet, not a
    /// script. Empty CSS clears any prior injection.
    CosmeticFilters(String),
    /// Set the page zoom percentage. `100` is normal size.
    SetZoom {
        /// Percent zoom, clamped by the shell before send.
        percent: u16,
    },
    /// Find text on the current page.
    FindInPage {
        /// Search query.
        query: String,
        /// Search backwards instead of forwards.
        backwards: bool,
    },
    /// Clear the current page-find highlight/selection where the helper supports it.
    ClearFind,
    /// Set whether page audio is muted for the tab.
    SetAudioMuted {
        /// `true` to mute audio, `false` to unmute.
        muted: bool,
    },
    /// Set whether the helper should force a dark page treatment for this tab.
    SetForceDark {
        /// `true` to install forced-dark styling, `false` to clear it.
        enabled: bool,
    },
    /// Set whether the helper should apply a reader-mode page treatment.
    SetReaderMode {
        /// `true` to install reader styling, `false` to clear it.
        enabled: bool,
    },
    /// Set whether the helper should run the shell-curated userscript bundle.
    SetUserScripts {
        /// `true` to install/run the bundle, `false` to clear its page effects.
        enabled: bool,
        /// The trusted shell-bundled JavaScript payload. Empty when clearing.
        bundle: String,
    },
    /// Override page-visible User-Agent metadata for the tab. Empty means the
    /// helper should restore its engine default.
    SetUserAgent {
        /// A bounded User-Agent string chosen by the shell.
        user_agent: String,
    },
    /// Override page-visible device metadata for the tab. `profile = "default"`
    /// asks the helper to restore its engine defaults where possible.
    SetDeviceProfile {
        /// Stable shell profile id, such as `phone` or `tablet`.
        profile: String,
        /// CSS viewport width exposed to page scripts.
        width: u16,
        /// CSS viewport height exposed to page scripts.
        height: u16,
        /// Device-pixel-ratio percentage; `100` means 1.0.
        scale_percent: u16,
        /// Whether page-visible touch capability should be exposed.
        touch: bool,
    },
    /// Ask the helper to print the current page.
    PrintPage,
    /// Ask the helper to save the current page as a PDF at `path`.
    SavePdf {
        /// Absolute output path owned by the shell.
        path: String,
    },
    /// Ask the helper to extract bounded visible page text for spellcheck/TTS.
    RequestPageText {
        /// Shell-minted request id echoed by [`EventMsg::PageText`].
        id: u64,
        /// Maximum UTF-8 bytes the helper may return.
        max_bytes: u32,
    },
    /// Ask the helper to extract a bounded active-page scrape body with visible
    /// text plus DOM-derived links/headings.
    RequestPageScrape {
        /// Shell-minted request id echoed by [`EventMsg::PageScrape`].
        id: u64,
        /// Maximum UTF-8 text bytes the helper may include.
        max_bytes: u32,
        /// Maximum DOM anchor records the helper may include.
        max_links: u16,
        /// Maximum heading records the helper may include.
        max_headings: u16,
    },
    /// Apply shell-owned spellcheck highlights to visible page text. Empty words
    /// clear prior highlights.
    SetSpellcheckHighlights {
        /// Bounded misspelled words from the offline Hunspell pass.
        words: Vec<String>,
    },
    /// Replace one shell-vetted spelling miss with a selected suggestion in the
    /// current page where the helper can find it.
    ApplySpellcheckCorrection {
        /// Misspelled word selected by the shell.
        word: String,
        /// Replacement suggestion selected by the operator.
        replacement: String,
    },
    /// Replace every visible occurrence the helper can safely match for one
    /// shell-vetted spelling miss.
    ApplySpellcheckCorrectionAll {
        /// Misspelled word selected by the shell.
        word: String,
        /// Replacement suggestion selected by the operator.
        replacement: String,
    },
    /// Replace one indexed visible occurrence for a shell-vetted spelling miss.
    ///
    /// The index is zero-based among visible matches for `word` in document order.
    ApplySpellcheckCorrectionAt {
        /// Misspelled word selected by the shell.
        word: String,
        /// Replacement suggestion selected by the operator.
        replacement: String,
        /// Zero-based visible occurrence index to replace.
        occurrence: u16,
    },
    /// Resolve or reject one page WebAuthn/passkey promise with daemon-owned
    /// credential/assertion material. The JSON body carries `client_request_id`
    /// so the helper resolves only the matching pending page request.
    CompletePasskey {
        /// Bounded daemon completion JSON.
        body: String,
    },
}

impl ControlMsg {
    /// Encode to the payload bytes (no length prefix — see [`frame`]).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Load(url) => {
                out.push(0);
                put_str(&mut out, url);
            }
            Self::Reload => out.push(1),
            Self::Stop => out.push(8),
            Self::Back => out.push(2),
            Self::Forward => out.push(3),
            Self::Resize { width, height } => {
                out.push(4);
                put_u32(&mut out, *width);
                put_u32(&mut out, *height);
            }
            Self::Input(ev) => {
                out.push(5);
                ev.encode(&mut out);
            }
            Self::ResourceVerdict { id, allow } => {
                out.push(6);
                put_u64(&mut out, *id);
                out.push(u8::from(*allow));
            }
            Self::CosmeticFilters(css) => {
                out.push(7);
                put_str(&mut out, css);
            }
            Self::SetZoom { percent } => {
                out.push(9);
                put_u16(&mut out, *percent);
            }
            Self::FindInPage { query, backwards } => {
                out.push(10);
                put_str(&mut out, query);
                out.push(u8::from(*backwards));
            }
            Self::ClearFind => out.push(11),
            Self::SetAudioMuted { muted } => {
                out.push(12);
                out.push(u8::from(*muted));
            }
            Self::SetForceDark { enabled } => {
                out.push(13);
                out.push(u8::from(*enabled));
            }
            Self::SetReaderMode { enabled } => {
                out.push(14);
                out.push(u8::from(*enabled));
            }
            Self::PrintPage => out.push(15),
            Self::SavePdf { path } => {
                out.push(16);
                put_str(&mut out, path);
            }
            Self::SetUserScripts { enabled, bundle } => {
                out.push(17);
                out.push(u8::from(*enabled));
                put_str(&mut out, bundle);
            }
            Self::RequestPageText { id, max_bytes } => {
                out.push(18);
                put_u64(&mut out, *id);
                put_u32(&mut out, *max_bytes);
            }
            Self::SetSpellcheckHighlights { words } => {
                out.push(19);
                put_string_vec(&mut out, words);
            }
            Self::ApplySpellcheckCorrection { word, replacement } => {
                out.push(20);
                put_str(&mut out, word);
                put_str(&mut out, replacement);
            }
            Self::ApplySpellcheckCorrectionAll { word, replacement } => {
                out.push(21);
                put_str(&mut out, word);
                put_str(&mut out, replacement);
            }
            Self::ApplySpellcheckCorrectionAt {
                word,
                replacement,
                occurrence,
            } => {
                out.push(22);
                put_str(&mut out, word);
                put_str(&mut out, replacement);
                put_u16(&mut out, *occurrence);
            }
            Self::CompletePasskey { body } => {
                out.push(23);
                put_str(&mut out, body);
            }
            Self::SetUserAgent { user_agent } => {
                out.push(24);
                put_str(&mut out, user_agent);
            }
            Self::SetDeviceProfile {
                profile,
                width,
                height,
                scale_percent,
                touch,
            } => {
                out.push(25);
                put_str(&mut out, profile);
                put_u16(&mut out, *width);
                put_u16(&mut out, *height);
                put_u16(&mut out, *scale_percent);
                out.push(u8::from(*touch));
            }
            Self::RequestPageScrape {
                id,
                max_bytes,
                max_links,
                max_headings,
            } => {
                out.push(26);
                put_u64(&mut out, *id);
                put_u32(&mut out, *max_bytes);
                put_u16(&mut out, *max_links);
                put_u16(&mut out, *max_headings);
            }
        }
        out
    }

    /// Decode from a single payload (no length prefix).
    ///
    /// # Errors
    /// Returns a [`WireError`] if the payload is truncated or carries an unknown
    /// tag / bad string.
    pub fn decode(payload: &[u8]) -> Result<Self, WireError> {
        let mut c = Cursor::new(payload);
        let msg = match c.u8()? {
            0 => Self::Load(c.string()?),
            1 => Self::Reload,
            2 => Self::Back,
            3 => Self::Forward,
            4 => Self::Resize {
                width: c.u32()?,
                height: c.u32()?,
            },
            5 => Self::Input(InputEvent::decode(&mut c)?),
            6 => Self::ResourceVerdict {
                id: c.u64()?,
                allow: c.bool()?,
            },
            7 => Self::CosmeticFilters(c.string()?),
            8 => Self::Stop,
            9 => Self::SetZoom { percent: c.u16()? },
            10 => Self::FindInPage {
                query: c.string()?,
                backwards: c.bool()?,
            },
            11 => Self::ClearFind,
            12 => Self::SetAudioMuted { muted: c.bool()? },
            13 => Self::SetForceDark { enabled: c.bool()? },
            14 => Self::SetReaderMode { enabled: c.bool()? },
            15 => Self::PrintPage,
            16 => Self::SavePdf { path: c.string()? },
            17 => Self::SetUserScripts {
                enabled: c.bool()?,
                bundle: c.string()?,
            },
            18 => Self::RequestPageText {
                id: c.u64()?,
                max_bytes: c.u32()?,
            },
            19 => Self::SetSpellcheckHighlights {
                words: c.string_vec()?,
            },
            20 => Self::ApplySpellcheckCorrection {
                word: c.string()?,
                replacement: c.string()?,
            },
            21 => Self::ApplySpellcheckCorrectionAll {
                word: c.string()?,
                replacement: c.string()?,
            },
            22 => Self::ApplySpellcheckCorrectionAt {
                word: c.string()?,
                replacement: c.string()?,
                occurrence: c.u16()?,
            },
            23 => Self::CompletePasskey { body: c.string()? },
            24 => Self::SetUserAgent {
                user_agent: c.string()?,
            },
            25 => Self::SetDeviceProfile {
                profile: c.string()?,
                width: c.u16()?,
                height: c.u16()?,
                scale_percent: c.u16()?,
                touch: c.bool()?,
            },
            26 => Self::RequestPageScrape {
                id: c.u64()?,
                max_bytes: c.u32()?,
                max_links: c.u16()?,
                max_headings: c.u16()?,
            },
            t => return Err(WireError::BadTag(t)),
        };
        Ok(msg)
    }
}

/// A message the helper sends back to the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventMsg {
    /// The shm frame region's fd is attached to THIS message via `SCM_RIGHTS`.
    /// Sent once, before the first [`Self::PaintReady`]; the receiver maps the
    /// carried descriptor read-only.
    AttachFrame,
    /// A fresh frame (sequence `seq`, always even/stable) is published on the shm
    /// channel — the shell reads it and uploads it to its texture. Frames are
    /// **not** streamed; this is the paint-ready signal.
    PaintReady {
        /// The published seqlock sequence (even = a stable frame).
        seq: u64,
    },
    /// The page title changed.
    Title(String),
    /// The navigation state changed (drives the chrome's back/forward/reload +
    /// address bar).
    NavState {
        /// A back-history entry exists.
        can_back: bool,
        /// A forward-history entry exists.
        can_forward: bool,
        /// A load is in progress.
        loading: bool,
        /// The committed URL.
        url: String,
    },
    /// The page/engine crashed; `reason` is a short human string.
    Crashed {
        /// Why it crashed.
        reason: String,
    },
    /// The helper is about to issue a subresource fetch and asks the shell's
    /// BOOKMARKS-7 ad-filter engine whether to proceed. The shell answers with a
    /// [`ControlMsg::ResourceVerdict`] carrying the same `id`.
    ResourceRequest {
        /// A helper-minted id the [`ControlMsg::ResourceVerdict`] echoes.
        id: u64,
        /// The full request URL.
        url: String,
        /// The request's resource class (the ABP `$type`) as the compact wire
        /// discriminant the shell's `resource_from_wire` maps back to
        /// `mde_adblock::ResourceType`.
        resource: u8,
    },
    /// A helper completed, or failed, a save-as-PDF request.
    PdfSaved {
        /// The requested output path.
        path: String,
        /// Whether the engine reported a successful PDF write.
        ok: bool,
    },
    /// Bounded visible page text extracted by the helper.
    PageText {
        /// Request id from [`ControlMsg::RequestPageText`].
        id: u64,
        /// Extracted visible text, possibly truncated to the requested byte cap.
        text: String,
    },
    /// Bounded active-page scrape body extracted by the helper.
    PageScrape {
        /// Request id from [`ControlMsg::RequestPageScrape`].
        id: u64,
        /// JSON object with visible text plus DOM-derived links/headings.
        body: String,
    },
    /// A page attempted a WebAuthn/passkey ceremony. The helper carries only
    /// public ceremony metadata as a bounded JSON object; the shell adds Browser
    /// source/engine/host fields and the daemon validates the final handoff.
    PasskeyRequest {
        /// JSON object with `ceremony`, `origin`, `rp_id`, challenge, and optional
        /// user / allow-credential metadata.
        body: String,
    },
    /// A browser-initiated download started or advanced (B2). One row in the shell's
    /// downloads drawer, keyed by `id`; re-sent on progress and at completion.
    Download {
        /// Helper-minted download id (stable across the download's lifetime).
        id: u64,
        /// The source URL being downloaded.
        url: String,
        /// The chosen/suggested file name (basename).
        filename: String,
        /// Bytes received so far.
        received: u64,
        /// Total bytes expected (0 if unknown).
        total: u64,
        /// The download finished writing successfully.
        done: bool,
        /// The download was canceled or interrupted.
        canceled: bool,
    },
    /// The page asked to open a new window/tab (window.open, target=_blank). The
    /// helper cancels the native popup (windowless CEF cannot host one) and asks
    /// the shell to open the URL as a regular tab instead.
    PopupRequested {
        /// The popup's target URL.
        url: String,
    },
    /// The engine changed the cursor shape (hover over a link, text field, resize
    /// edge, …) so the shell can reflect it instead of a static arrow.
    CursorChanged {
        /// The engine-neutral cursor shape.
        kind: CursorKind,
    },
}

impl EventMsg {
    /// Encode to the payload bytes (no length prefix).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::AttachFrame => out.push(0),
            Self::PaintReady { seq } => {
                out.push(1);
                put_u64(&mut out, *seq);
            }
            Self::Title(t) => {
                out.push(2);
                put_str(&mut out, t);
            }
            Self::NavState {
                can_back,
                can_forward,
                loading,
                url,
            } => {
                out.push(3);
                out.push(u8::from(*can_back));
                out.push(u8::from(*can_forward));
                out.push(u8::from(*loading));
                put_str(&mut out, url);
            }
            Self::Crashed { reason } => {
                out.push(4);
                put_str(&mut out, reason);
            }
            Self::ResourceRequest { id, url, resource } => {
                out.push(5);
                put_u64(&mut out, *id);
                put_str(&mut out, url);
                out.push(*resource);
            }
            Self::PdfSaved { path, ok } => {
                out.push(6);
                put_str(&mut out, path);
                out.push(u8::from(*ok));
            }
            Self::PageText { id, text } => {
                out.push(7);
                put_u64(&mut out, *id);
                put_str(&mut out, text);
            }
            Self::PasskeyRequest { body } => {
                out.push(8);
                put_str(&mut out, body);
            }
            Self::PageScrape { id, body } => {
                out.push(9);
                put_u64(&mut out, *id);
                put_str(&mut out, body);
            }
            Self::Download {
                id,
                url,
                filename,
                received,
                total,
                done,
                canceled,
            } => {
                out.push(10);
                put_u64(&mut out, *id);
                put_str(&mut out, url);
                put_str(&mut out, filename);
                put_u64(&mut out, *received);
                put_u64(&mut out, *total);
                out.push(u8::from(*done));
                out.push(u8::from(*canceled));
            }
            Self::PopupRequested { url } => {
                out.push(11);
                put_str(&mut out, url);
            }
            Self::CursorChanged { kind } => {
                out.push(12);
                out.push(*kind as u8);
            }
        }
        out
    }

    /// Decode from a single payload (no length prefix).
    ///
    /// # Errors
    /// Returns a [`WireError`] if the payload is truncated or carries an unknown
    /// tag / bad string.
    pub fn decode(payload: &[u8]) -> Result<Self, WireError> {
        let mut c = Cursor::new(payload);
        let msg = match c.u8()? {
            0 => Self::AttachFrame,
            1 => Self::PaintReady { seq: c.u64()? },
            2 => Self::Title(c.string()?),
            3 => Self::NavState {
                can_back: c.bool()?,
                can_forward: c.bool()?,
                loading: c.bool()?,
                url: c.string()?,
            },
            4 => Self::Crashed {
                reason: c.string()?,
            },
            5 => Self::ResourceRequest {
                id: c.u64()?,
                url: c.string()?,
                resource: c.u8()?,
            },
            6 => Self::PdfSaved {
                path: c.string()?,
                ok: c.bool()?,
            },
            7 => Self::PageText {
                id: c.u64()?,
                text: c.string()?,
            },
            8 => Self::PasskeyRequest { body: c.string()? },
            9 => Self::PageScrape {
                id: c.u64()?,
                body: c.string()?,
            },
            10 => Self::Download {
                id: c.u64()?,
                url: c.string()?,
                filename: c.string()?,
                received: c.u64()?,
                total: c.u64()?,
                done: c.bool()?,
                canceled: c.bool()?,
            },
            11 => Self::PopupRequested { url: c.string()? },
            12 => Self::CursorChanged {
                kind: CursorKind::from_u8(c.u8()?),
            },
            t => return Err(WireError::BadTag(t)),
        };
        Ok(msg)
    }
}

/// Wrap a payload in the on-wire length prefix: `[u32 LE len][payload]`.
#[must_use]
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    put_len(&mut out, payload.len());
    out.extend_from_slice(payload);
    out
}

/// Pop one complete length-prefixed frame's payload off the front of `buf`,
/// draining the consumed bytes. Returns:
///
/// * `Ok(Some(payload))` — a full frame was available and removed;
/// * `Ok(None)` — not enough bytes buffered yet (leave `buf` intact, read more);
/// * `Err` — the length prefix exceeds [`MAX_FRAME_LEN`] (a corrupt stream).
///
/// # Errors
/// [`WireError::TooLong`] if the declared length is over the cap.
pub fn take_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, WireError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(WireError::TooLong(len));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let payload = buf[4..4 + len].to_vec();
    buf.drain(..4 + len);
    Ok(Some(payload))
}

// ── Primitive put/get helpers ────────────────────────────────────────────────

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}
/// Write a byte-length as a `u32` prefix. Lengths on this wire are bounded by
/// [`MAX_FRAME_LEN`] (and a `u32`-length frame is rejected on decode), so a
/// buffer large enough to truncate cannot occur; a saturating cast keeps the
/// helper panic-free.
fn put_len(out: &mut Vec<u8>, len: usize) {
    put_u32(out, u32::try_from(len).unwrap_or(u32::MAX));
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}
fn put_string_vec(out: &mut Vec<u8>, values: &[String]) {
    put_len(out, values.len());
    for value in values {
        put_str(out, value);
    }
}

/// A bounds-checked forward reader over a payload.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::UnexpectedEnd)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(WireError::UnexpectedEnd)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool, WireError> {
        Ok(self.u8()? != 0)
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn f32(&mut self) -> Result<f32, WireError> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn string(&mut self) -> Result<String, WireError> {
        let len = self.u32()? as usize;
        if len > MAX_FRAME_LEN {
            return Err(WireError::TooLong(len));
        }
        let b = self.take(len)?;
        std::str::from_utf8(b)
            .map(ToOwned::to_owned)
            .map_err(|_| WireError::BadUtf8)
    }
    fn string_vec(&mut self) -> Result<Vec<String>, WireError> {
        let len = self.u32()? as usize;
        if len > MAX_FRAME_LEN {
            return Err(WireError::TooLong(len));
        }
        let mut values = Vec::with_capacity(len.min(256));
        for _ in 0..len {
            values.push(self.string()?);
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_control(msg: &ControlMsg) {
        let payload = msg.encode();
        assert_eq!(&ControlMsg::decode(&payload).expect("decode"), msg);
        // And through the length-prefix framing + streaming parser.
        let mut buf = frame(&payload);
        let popped = take_frame(&mut buf).expect("no error").expect("one frame");
        assert!(buf.is_empty(), "the framer left trailing bytes");
        assert_eq!(&ControlMsg::decode(&popped).expect("decode"), msg);
    }

    fn round_event(msg: &EventMsg) {
        let payload = msg.encode();
        assert_eq!(&EventMsg::decode(&payload).expect("decode"), msg);
    }

    #[test]
    fn control_messages_round_trip() {
        round_control(&ControlMsg::Load("https://example.test/a?b=1".to_owned()));
        round_control(&ControlMsg::Reload);
        round_control(&ControlMsg::Stop);
        round_control(&ControlMsg::Back);
        round_control(&ControlMsg::Forward);
        round_control(&ControlMsg::Resize {
            width: 1280,
            height: 800,
        });
        round_control(&ControlMsg::Input(InputEvent::PointerMoved {
            x: 12.5,
            y: 7.25,
        }));
        round_control(&ControlMsg::Input(InputEvent::PointerButton {
            x: 3.0,
            y: 4.0,
            button: PointerButton::Secondary,
            pressed: true,
            modifiers: Modifiers(Modifiers::CTRL | Modifiers::SHIFT),
        }));
        round_control(&ControlMsg::Input(InputEvent::PointerGone));
        round_control(&ControlMsg::Input(InputEvent::Scroll {
            delta_x: -2.0,
            delta_y: 40.0,
            modifiers: Modifiers(Modifiers::CTRL),
        }));
        round_control(&ControlMsg::Input(InputEvent::Key {
            key: KeyCode::Enter,
            pressed: true,
            modifiers: Modifiers(Modifiers::CTRL | Modifiers::SHIFT),
        }));
        round_control(&ControlMsg::Input(InputEvent::Text("héllo →".to_owned())));
        // BOOKMARKS-7 ad-filter control messages.
        round_control(&ControlMsg::ResourceVerdict {
            id: 7,
            allow: false,
        });
        round_control(&ControlMsg::ResourceVerdict { id: 9, allow: true });
        round_control(&ControlMsg::CosmeticFilters(
            ".ad, #banner { display: none !important; }".to_owned(),
        ));
        round_control(&ControlMsg::CosmeticFilters(String::new()));
        round_control(&ControlMsg::SetZoom { percent: 125 });
        round_control(&ControlMsg::FindInPage {
            query: "mesh".to_owned(),
            backwards: false,
        });
        round_control(&ControlMsg::FindInPage {
            query: "mesh".to_owned(),
            backwards: true,
        });
        round_control(&ControlMsg::ClearFind);
        round_control(&ControlMsg::SetAudioMuted { muted: true });
        round_control(&ControlMsg::SetAudioMuted { muted: false });
        round_control(&ControlMsg::SetForceDark { enabled: true });
        round_control(&ControlMsg::SetForceDark { enabled: false });
        round_control(&ControlMsg::SetReaderMode { enabled: true });
        round_control(&ControlMsg::SetReaderMode { enabled: false });
        round_control(&ControlMsg::SetUserScripts {
            enabled: true,
            bundle: "console.log('mde');".to_owned(),
        });
        round_control(&ControlMsg::SetUserScripts {
            enabled: false,
            bundle: String::new(),
        });
        round_control(&ControlMsg::SetUserAgent {
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36".to_owned(),
        });
        round_control(&ControlMsg::SetUserAgent {
            user_agent: String::new(),
        });
        round_control(&ControlMsg::SetDeviceProfile {
            profile: "phone".to_owned(),
            width: 390,
            height: 844,
            scale_percent: 300,
            touch: true,
        });
        round_control(&ControlMsg::SetDeviceProfile {
            profile: "default".to_owned(),
            width: 0,
            height: 0,
            scale_percent: 100,
            touch: false,
        });
        round_control(&ControlMsg::PrintPage);
        round_control(&ControlMsg::SavePdf {
            path: "/tmp/mde-page.pdf".to_owned(),
        });
        round_control(&ControlMsg::RequestPageText {
            id: 42,
            max_bytes: 16 * 1024,
        });
        round_control(&ControlMsg::RequestPageScrape {
            id: 43,
            max_bytes: 64 * 1024,
            max_links: 64,
            max_headings: 32,
        });
        round_control(&ControlMsg::SetSpellcheckHighlights {
            words: vec!["wrold".to_owned(), "msh".to_owned()],
        });
        round_control(&ControlMsg::SetSpellcheckHighlights { words: Vec::new() });
        round_control(&ControlMsg::ApplySpellcheckCorrection {
            word: "wrold".to_owned(),
            replacement: "world".to_owned(),
        });
        round_control(&ControlMsg::ApplySpellcheckCorrectionAll {
            word: "wrold".to_owned(),
            replacement: "world".to_owned(),
        });
        round_control(&ControlMsg::ApplySpellcheckCorrectionAt {
            word: "wrold".to_owned(),
            replacement: "world".to_owned(),
            occurrence: 3,
        });
        round_control(&ControlMsg::CompletePasskey {
            body: r#"{"client_request_id":"pk-1","op":"browser_passkey_assertion"}"#.to_owned(),
        });
    }

    #[test]
    fn event_messages_round_trip() {
        round_event(&EventMsg::AttachFrame);
        round_event(&EventMsg::PaintReady { seq: 42 });
        round_event(&EventMsg::Title("A Page".to_owned()));
        round_event(&EventMsg::NavState {
            can_back: true,
            can_forward: false,
            loading: true,
            url: "https://example.test/".to_owned(),
        });
        round_event(&EventMsg::Crashed {
            reason: "engine SIGSEGV".to_owned(),
        });
        round_event(&EventMsg::ResourceRequest {
            id: 42,
            url: "https://doubleclick.net/pixel.gif".to_owned(),
            resource: 4,
        });
        round_event(&EventMsg::PdfSaved {
            path: "/tmp/mde-page.pdf".to_owned(),
            ok: true,
        });
        round_event(&EventMsg::PdfSaved {
            path: "/tmp/mde-page.pdf".to_owned(),
            ok: false,
        });
        round_event(&EventMsg::PageText {
            id: 42,
            text: "hello page".to_owned(),
        });
        round_event(&EventMsg::PageScrape {
            id: 43,
            body: r#"{"text":"hello","links":[],"headings":[]}"#.to_owned(),
        });
        round_event(&EventMsg::PasskeyRequest {
            body: r#"{"ceremony":"create","origin":"https://login.example"}"#.to_owned(),
        });
        round_event(&EventMsg::Download {
            id: 7,
            url: "https://files.example/report.pdf".to_owned(),
            filename: "report.pdf".to_owned(),
            received: 2048,
            total: 65536,
            done: false,
            canceled: false,
        });
        round_event(&EventMsg::Download {
            id: 7,
            url: "https://files.example/report.pdf".to_owned(),
            filename: "report.pdf".to_owned(),
            received: 65536,
            total: 65536,
            done: true,
            canceled: false,
        });
        round_event(&EventMsg::PopupRequested {
            url: "https://example.com/window-open-target".to_owned(),
        });
        round_event(&EventMsg::CursorChanged {
            kind: CursorKind::Pointer,
        });
        round_event(&EventMsg::CursorChanged {
            kind: CursorKind::ResizeNwSe,
        });
        // Unknown wire bytes decode to the default cursor, never an error.
        assert_eq!(CursorKind::from_u8(200), CursorKind::Default);
    }

    #[test]
    fn attach_and_paint_ready_encode_to_the_pinned_golden() {
        // The exact bytes the sandboxed helper (`mde-web-preview`) must reproduce —
        // it shares THIS file, but pinning the golden on both ends turns an
        // accidental un-share-and-drift into a red test rather than a silent
        // "stuck on Loading the page…" regression. Mirrored in the helper's
        // `tests/protocol_golden.rs`.
        assert_eq!(EventMsg::AttachFrame.encode(), vec![0u8]);
        assert_eq!(
            EventMsg::PaintReady {
                seq: 0x0102_0304_0506_0708
            }
            .encode(),
            vec![1, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
        );
    }

    #[test]
    fn every_keycode_round_trips_its_discriminant() {
        for raw in 0u16..=62 {
            let key = KeyCode::from_u16(raw).expect("dense 0..=62 range");
            assert_eq!(key.as_u16(), raw);
        }
        assert_eq!(KeyCode::from_u16(63), None, "out-of-range key rejected");
    }

    #[test]
    fn a_truncated_payload_is_a_typed_error_not_a_panic() {
        assert_eq!(ControlMsg::decode(&[]), Err(WireError::UnexpectedEnd));
        // Load with a length claiming 8 bytes but none present.
        let mut bad = vec![0u8]; // tag Load
        bad.extend_from_slice(&8u32.to_le_bytes());
        assert_eq!(ControlMsg::decode(&bad), Err(WireError::UnexpectedEnd));
        assert!(matches!(
            ControlMsg::decode(&[99]),
            Err(WireError::BadTag(99))
        ));
    }

    #[test]
    fn take_frame_reassembles_across_partial_reads() {
        // Two frames concatenated, delivered in three arbitrary chunks.
        let a = frame(&ControlMsg::Reload.encode());
        let b = frame(&ControlMsg::Load("x".to_owned()).encode());
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(&a);
        stream.extend_from_slice(&b);

        // Feed only the first 2 bytes: not a whole length prefix yet.
        let mut buf = stream[..2].to_vec();
        assert_eq!(take_frame(&mut buf).expect("ok"), None);
        // Now the rest arrives.
        buf.extend_from_slice(&stream[2..]);
        let f1 = take_frame(&mut buf).expect("ok").expect("frame 1");
        assert_eq!(ControlMsg::decode(&f1).expect("decode"), ControlMsg::Reload);
        let f2 = take_frame(&mut buf).expect("ok").expect("frame 2");
        assert_eq!(
            ControlMsg::decode(&f2).expect("decode"),
            ControlMsg::Load("x".to_owned())
        );
        assert!(take_frame(&mut buf).expect("ok").is_none());
    }

    #[test]
    fn take_frame_rejects_an_absurd_length_prefix() {
        // u32::MAX is far above MAX_FRAME_LEN (1 MiB), so the framer rejects it.
        let mut buf = u32::MAX.to_le_bytes().to_vec();
        assert!(matches!(take_frame(&mut buf), Err(WireError::TooLong(_))));
    }
}

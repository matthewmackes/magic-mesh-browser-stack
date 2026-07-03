//! [`ResourceType`] — the request classes a network rule can scope to, matching
//! the Adblock Plus `$type` option names (BOOKMARKS-7).
//!
//! A filter line `||track.example^$script,image` blocks only script + image
//! requests; a request carries exactly one [`ResourceType`], and the matcher
//! ([`crate::Engine`]) checks it against a rule's include/exclude type mask.

use serde::{Deserialize, Serialize};

/// The class of a network request, mirroring the ABP `$type` filter options.
///
/// One request has exactly one type; a [`crate::NetworkRule`] may restrict which
/// types it applies to (`$script`, `$~image`, …). The variants and their filter
/// keywords are the common EasyList/uBlock subset — a keyword the parser does
/// not recognise makes the whole rule unsupported (parsed but never matched),
/// which is honest rather than silently mis-scoping it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ResourceType {
    /// The top-level page navigation (`$document`).
    Document,
    /// A nested frame / iframe navigation (`$subdocument`).
    Subdocument,
    /// A CSS stylesheet (`$stylesheet`, alias `$css`).
    Stylesheet,
    /// A script (`$script`).
    Script,
    /// An image (`$image`).
    Image,
    /// A web font (`$font`).
    Font,
    /// Audio/video media (`$media`).
    Media,
    /// A plugin object / `<embed>` (`$object`).
    Object,
    /// An `XMLHttpRequest` / `fetch` (`$xmlhttprequest`, alias `$xhr`).
    XmlHttpRequest,
    /// A hyperlink auditing / beacon ping (`$ping`, alias `$beacon`).
    Ping,
    /// A WebSocket handshake (`$websocket`).
    WebSocket,
    /// Anything that fits none of the above (`$other`).
    Other,
}

impl ResourceType {
    /// The single-bit mask for this type, used by [`crate::RuleOptions`] to hold
    /// an include/exclude type set in one `u16` without an allocation.
    #[must_use]
    pub(crate) const fn bit(self) -> u16 {
        1u16 << (self as u16)
    }

    /// Parse an ABP `$type` option keyword into a [`ResourceType`].
    ///
    /// Accepts the common aliases (`css` → [`Self::Stylesheet`], `xhr` →
    /// [`Self::XmlHttpRequest`], `beacon` → [`Self::Ping`]). Returns `None` for
    /// an unknown keyword so the caller can mark the rule unsupported.
    #[must_use]
    pub fn from_option(keyword: &str) -> Option<Self> {
        let ty = match keyword {
            "document" | "doc" => Self::Document,
            "subdocument" | "frame" => Self::Subdocument,
            "stylesheet" | "css" => Self::Stylesheet,
            "script" => Self::Script,
            "image" | "img" => Self::Image,
            "font" => Self::Font,
            "media" => Self::Media,
            "object" | "object-subrequest" => Self::Object,
            "xmlhttprequest" | "xhr" => Self::XmlHttpRequest,
            "ping" | "beacon" => Self::Ping,
            "websocket" => Self::WebSocket,
            "other" => Self::Other,
            _ => return None,
        };
        Some(ty)
    }
}

/// A compact include/exclude set of [`ResourceType`]s held in two `u16` masks.
///
/// `include` empty (zero) means "all types"; a non-empty `include` restricts the
/// rule to those types. `exclude` removes types (the `$~script` negation). A
/// request type matches when it is in `include` (or `include` is empty) **and**
/// not in `exclude`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ResourceMask {
    include: u16,
    exclude: u16,
}

impl ResourceMask {
    /// Add `ty` to the positive (include) set.
    pub(crate) const fn include(&mut self, ty: ResourceType) {
        self.include |= ty.bit();
    }

    /// Add `ty` to the negative (exclude) set — the `$~type` negation.
    pub(crate) const fn exclude(&mut self, ty: ResourceType) {
        self.exclude |= ty.bit();
    }

    /// Does a request of type `ty` satisfy this mask?
    pub(crate) const fn matches(self, ty: ResourceType) -> bool {
        let bit = ty.bit();
        (self.exclude & bit) == 0 && (self.include == 0 || (self.include & bit) != 0)
    }
}

//! Minimal, dependency-free URL helpers for filter matching (BOOKMARKS-7).
//!
//! The engine only needs three facts about a request URL: its host (for the
//! `||domain^` anchor + the third-party test), a coarse *registrable domain*
//! (for the third-party / `domain=` comparison), and whether request-vs-page are
//! same-site. A full URL parser (the `url` crate) is deliberately avoided to
//! keep the crate std-only and airgap-trivial; these helpers cover the shapes an
//! adblocker matches against (`scheme://host[:port]/path?query#frag`).

/// Extract the host component of a URL.
///
/// Returns the lowercased authority host between `://` and the first `/`, `?`,
/// `#` or `:` (the port). Userinfo (`user@host`) is stripped. Returns `None` for
/// a URL with no `://` authority (e.g. a bare `about:blank` or `data:` URL,
/// which carries no host to anchor a domain rule to).
#[must_use]
pub fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    // Authority ends at the first path/query/fragment delimiter.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Drop any userinfo (`user:pass@host`).
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Drop the port. IPv6 literals (`[::1]:80`) keep their brackets' contents.
    let host = hostport.strip_prefix('[').map_or_else(
        || hostport.split_once(':').map_or(hostport, |(h, _)| h),
        |rest| rest.split_once(']').map_or(rest, |(h, _)| h),
    );
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// A coarse *registrable domain* heuristic: the last two dot-labels of a host.
///
/// `sub.tracker.example.com` → `example.com`; `example.com` → `example.com`; a
/// bare `localhost` or an IP literal returns itself. This is intentionally a
/// heuristic — a true registrable domain needs the Public Suffix List (a large
/// vendored blob), so multi-label public suffixes (`example.co.uk`) collapse to
/// `co.uk`. That is documented and acceptable for the common-subset engine; the
/// third-party test it feeds is a best-effort signal, never a security boundary.
#[must_use]
pub fn registrable_domain(host: &str) -> String {
    let host = host.trim_end_matches('.');
    // IPv6 literal or IP: no dot-label registrable domain — use it whole.
    if host.contains(':') || host.parse::<std::net::Ipv4Addr>().is_ok() {
        return host.to_ascii_lowercase();
    }
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 2 {
        return host.to_ascii_lowercase();
    }
    labels[labels.len() - 2..].join(".").to_ascii_lowercase()
}

/// Is `request_host` third-party relative to the `first_party` page domain?
///
/// True when their [`registrable_domain`]s differ. `first_party` may be a bare
/// domain or a full host; both are reduced to their registrable domain first.
/// An empty `first_party` (no page context) is treated as same-site so a rule
/// gated on `$third-party` does not fire spuriously.
#[must_use]
pub fn is_third_party(request_host: &str, first_party: &str) -> bool {
    if first_party.is_empty() {
        return false;
    }
    registrable_domain(request_host) != registrable_domain(first_party)
}

/// Does `host` equal `domain` or sit under it as a subdomain?
///
/// `is_subdomain_of("a.example.com", "example.com")` is true; `"example.com"`
/// itself is true; `"notexample.com"` is false (the label boundary is enforced).
#[must_use]
pub fn is_subdomain_of(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
}

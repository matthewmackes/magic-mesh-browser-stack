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

/// Why [`confusable_reason`] flagged a hostname as a homograph/spoofing risk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfusableReason {
    /// A single dot-label mixes an ASCII Latin letter with a letter from
    /// another (non-ASCII) script — e.g. `"а"` (Cyrillic) + `"pple"` (Latin).
    MixedScript,
    /// A refinement of [`Self::MixedScript`]: the non-ASCII letters come from
    /// the Cyrillic or Greek block specifically, the two scripts most
    /// commonly used to spoof Latin lookalike letters (`а`/`a`, `е`/`e`,
    /// `о`/`o`, Greek `ο`, …).
    ConfusableBlock,
    /// A label uses the `xn--` Punycode ACE prefix, i.e. it is an IDN
    /// (Internationalized Domain Name) encoded for the DNS wire format.
    Punycode,
}

/// Is `host` a likely homograph/spoofing risk?
///
/// Delegates to [`confusable_reason`]; see it for the detection rules. This
/// is the cheap yes/no the URL bar can gate a warning icon on.
#[must_use]
pub fn is_confusable_host(host: &str) -> bool {
    confusable_reason(host).is_some()
}

/// Classify `host` as a homograph/spoofing risk, if it is one.
///
/// Checked **per label** (a host is split on `.`):
///
///   * A label mixing an ASCII Latin letter (`a-z`/`A-Z`) with a non-ASCII
///     letter (`char::is_alphabetic() && !is_ascii()`) is flagged
///     [`ConfusableReason::MixedScript`] — legitimate domains essentially
///     never mix ASCII Latin with another script inside one label, so this
///     is the classic `"аpple.com"` (Cyrillic `а` + Latin `pple`) attack.
///     When the non-ASCII letters are specifically Cyrillic (U+0400–U+04FF)
///     or Greek (U+0370–U+03FF) — the two blocks with the most Latin
///     lookalikes — the more specific [`ConfusableReason::ConfusableBlock`]
///     is returned instead.
///   * A **whole** non-Latin label (no ASCII Latin letters in it at all,
///     e.g. a genuinely Russian `почта.com`) is *not* flagged — each label
///     is single-script, so there is nothing to confuse it with within that
///     label. Cross-label script switches (`почта.com`) are common and
///     legitimate; only an in-label mix is a spoofing tell.
///   * A label starting with `xn--` (the Punycode ASCII-Compatible Encoding
///     prefix, checked case-insensitively) is flagged
///     [`ConfusableReason::Punycode`]. **Limitation:** this crate does not
///     decode Punycode (no dependency budget — see the module docs), so a
///     `xn--` label is flagged unconditionally as "IDN, verify" rather than
///     decoded and re-checked for an actual script mix. That is the intended
///     v1 behavior: surfacing every IDN label to the omnibox/UI for the user
///     to eyeball is cheap and safe, even though it will also flag benign
///     IDNs (no false negatives, some false positives).
///
/// Returns `None` for an empty host or one with only empty labels (e.g. a
/// bare `"."`) — nothing to check, not a panic.
#[must_use]
pub fn confusable_reason(host: &str) -> Option<ConfusableReason> {
    host.trim_end_matches('.')
        .split('.')
        .filter(|label| !label.is_empty())
        .find_map(label_confusable_reason)
}

/// Per-label check backing [`confusable_reason`]; see it for the rules.
fn label_confusable_reason(label: &str) -> Option<ConfusableReason> {
    // Punycode ACE prefix, checked byte-safely (`get` avoids panicking on a
    // non-char-boundary index) and case-insensitively.
    if label
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("xn--"))
    {
        return Some(ConfusableReason::Punycode);
    }
    // All-ASCII labels (the overwhelming common case) can't mix scripts —
    // skip the char scan entirely.
    if label.is_ascii() {
        return None;
    }
    let mut has_ascii_latin = false;
    let mut has_confusable_block = false;
    let mut has_other_non_ascii_alpha = false;
    for c in label.chars() {
        if c.is_ascii_alphabetic() {
            has_ascii_latin = true;
        } else if !c.is_ascii() && c.is_alphabetic() {
            if is_confusable_block_char(c) {
                has_confusable_block = true;
            } else {
                has_other_non_ascii_alpha = true;
            }
        }
    }
    if has_ascii_latin && has_confusable_block {
        Some(ConfusableReason::ConfusableBlock)
    } else if has_ascii_latin && has_other_non_ascii_alpha {
        Some(ConfusableReason::MixedScript)
    } else {
        None
    }
}

/// Is `c` in the Cyrillic (U+0400–U+04FF) or Greek (U+0370–U+03FF) block —
/// the two scripts most commonly used for Latin-lookalike homographs?
fn is_confusable_block_char(c: char) -> bool {
    matches!(c as u32, 0x0370..=0x03FF | 0x0400..=0x04FF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_hosts_are_never_confusable() {
        assert!(!is_confusable_host("apple.com"));
        assert!(!is_confusable_host("sub.example.co.uk"));
        assert!(!is_confusable_host("127.0.0.1"));
        assert_eq!(confusable_reason("apple.com"), None);
    }

    #[test]
    fn cyrillic_latin_mix_in_one_label_is_confusable_block() {
        // Cyrillic "а" (U+0430) + Latin "pple" inside one label.
        let host = "\u{0430}pple.com";
        assert!(is_confusable_host(host));
        assert_eq!(
            confusable_reason(host),
            Some(ConfusableReason::ConfusableBlock)
        );
    }

    #[test]
    fn greek_latin_mix_in_one_label_is_confusable_block() {
        // Greek "ο" (U+03BF, omicron) + Latin "nline" inside one label.
        let host = "\u{03bf}nline-bank.com";
        assert!(is_confusable_host(host));
        assert_eq!(
            confusable_reason(host),
            Some(ConfusableReason::ConfusableBlock)
        );
    }

    #[test]
    fn whole_cyrillic_label_with_ascii_tld_is_not_flagged() {
        // "почта.com" — the label itself is entirely Cyrillic (a genuine
        // Russian-script domain), so no in-label script mix exists. The TLD
        // label "com" is plain ASCII. Neither label mixes scripts, so this
        // is deliberately NOT flagged — cross-label script switches are
        // normal for legitimate non-Latin-script domains.
        let host = "\u{043f}\u{043e}\u{0447}\u{0442}\u{0430}.com";
        assert!(!is_confusable_host(host));
        assert_eq!(confusable_reason(host), None);
    }

    #[test]
    fn punycode_label_is_flagged_as_idn_verify() {
        let host = "xn--80ak6aa92e.com";
        assert!(is_confusable_host(host));
        assert_eq!(confusable_reason(host), Some(ConfusableReason::Punycode));
    }

    #[test]
    fn punycode_prefix_is_case_insensitive() {
        assert_eq!(
            confusable_reason("XN--80ak6aa92e.com"),
            Some(ConfusableReason::Punycode)
        );
    }

    #[test]
    fn empty_host_and_trailing_dot_do_not_panic() {
        assert_eq!(confusable_reason(""), None);
        assert_eq!(confusable_reason("."), None);
        assert!(!is_confusable_host(""));
        assert!(!is_confusable_host("."));
        // Trailing dot on an otherwise-safe host: still safe.
        assert!(!is_confusable_host("apple.com."));
        // Trailing dot doesn't hide a confusable label.
        assert!(is_confusable_host("xn--80ak6aa92e.com."));
    }

    #[test]
    fn latin_plus_non_confusable_script_is_still_mixed_script() {
        // Armenian "ա" (U+0561) + Latin "pple" — outside the Cyrillic/Greek
        // blocks, so this is the general MixedScript signal, not the more
        // specific ConfusableBlock one.
        let host = "\u{0561}pple.com";
        assert!(is_confusable_host(host));
        assert_eq!(confusable_reason(host), Some(ConfusableReason::MixedScript));
    }
}

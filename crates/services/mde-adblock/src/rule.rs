//! [`parse_line`] and the typed rules it yields (BOOKMARKS-7).
//!
//! One filter-list line parses into a [`ParsedLine`]: a [`NetworkRule`] (a
//! block or `@@` allow/exception pattern with its `$options`), a
//! [`CosmeticRule`] (a `##` element-hide or `#@#` un-hide selector, optionally
//! domain-scoped), a comment, a blank, or an [`ParsedLine::Unsupported`] line
//! (a shape we recognise but deliberately do not match — a `/regex/` body, a
//! scriptlet-inject `#$#`, or a rule carrying an option we do not model). Marking
//! the not-fully-understood lines unsupported keeps the matcher **correct on the
//! subset** rather than silently mis-applying a half-parsed rule (§7).

use crate::pattern::Pattern;
use crate::resource::{ResourceMask, ResourceType};
use crate::url::is_subdomain_of;

/// The `$options` of a [`NetworkRule`]: which requests it is scoped to.
#[derive(Clone, Debug, Default)]
pub struct RuleOptions {
    /// The resource-type include/exclude set (`$script`, `$~image`); empty
    /// include means "any type".
    types: ResourceMask,
    /// `Some(true)` = only third-party requests (`$third-party`); `Some(false)` =
    /// only first-party (`$~third-party`); `None` = either.
    third_party: Option<bool>,
    /// `$domain=a.com|b.com`: the rule applies only on these first-party pages.
    domains_include: Vec<String>,
    /// `$domain=~a.com`: the rule does not apply on these first-party pages.
    domains_exclude: Vec<String>,
}

impl RuleOptions {
    /// Does a request of `resource_type`, from a page whose first-party host is
    /// `first_party`, with third-party status `third_party`, fall in this rule's
    /// scope? (The URL-pattern match is checked separately by the engine.)
    #[must_use]
    pub fn applies(
        &self,
        resource_type: ResourceType,
        first_party: &str,
        third_party: bool,
    ) -> bool {
        if !self.types.matches(resource_type) {
            return false;
        }
        if let Some(want_third) = self.third_party {
            if want_third != third_party {
                return false;
            }
        }
        if !self.domains_include.is_empty()
            && !self
                .domains_include
                .iter()
                .any(|d| is_subdomain_of(first_party, d))
        {
            return false;
        }
        if self
            .domains_exclude
            .iter()
            .any(|d| is_subdomain_of(first_party, d))
        {
            return false;
        }
        true
    }
}

/// A parsed network filter: a URL [`Pattern`] plus its scope [`RuleOptions`].
///
/// A blocking rule (`is_exception == false`) blocks a matching request; an
/// exception rule (`@@…`, `is_exception == true`) allows one, overriding blocks.
#[derive(Clone, Debug)]
pub struct NetworkRule {
    /// The original filter line, kept for the blocked-count UI / logs.
    pub raw: String,
    /// The compiled URL match pattern.
    pub pattern: Pattern,
    /// An `@@` allow/exception rule (overrides block rules).
    pub is_exception: bool,
    /// The request-scope options.
    pub options: RuleOptions,
}

/// A parsed cosmetic (element-hide) filter.
///
/// `##.ad-banner` hides everywhere; `example.com##.promo` hides only on that
/// domain (+ subdomains); `~example.com##.promo` hides everywhere except there;
/// `example.com#@#.promo` (`is_exception`) un-hides a selector another rule hides.
#[derive(Clone, Debug)]
pub struct CosmeticRule {
    /// The CSS selector to hide (or un-hide) elements matching.
    pub selector: String,
    /// Domains the rule is scoped to; empty means generic (all sites).
    domains_include: Vec<String>,
    /// Domains the rule is explicitly excluded from.
    domains_exclude: Vec<String>,
    /// A `#@#` un-hide exception.
    pub is_exception: bool,
}

impl CosmeticRule {
    /// Does this cosmetic rule apply on `host`?
    ///
    /// A generic rule (no include domains) applies unless `host` is excluded; a
    /// domain-scoped rule applies only when `host` is (a subdomain of) an include
    /// domain and not excluded.
    #[must_use]
    pub fn applies_to(&self, host: &str) -> bool {
        if self
            .domains_exclude
            .iter()
            .any(|d| is_subdomain_of(host, d))
        {
            return false;
        }
        if self.domains_include.is_empty() {
            return true;
        }
        self.domains_include
            .iter()
            .any(|d| is_subdomain_of(host, d))
    }
}

/// The outcome of parsing one filter-list line.
#[derive(Clone, Debug)]
pub enum ParsedLine {
    /// A network block / allow rule.
    Network(NetworkRule),
    /// A cosmetic element-hide / un-hide rule.
    Cosmetic(CosmeticRule),
    /// A comment (`! …`) or the `[Adblock Plus 2.0]` header.
    Comment,
    /// A blank line.
    Blank,
    /// A recognised-but-unmodelled line, kept as its raw text so
    /// [`crate::ParseStats`] can honestly count what was skipped.
    Unsupported(String),
}

/// Parse one filter-list line into a [`ParsedLine`].
#[must_use]
pub fn parse_line(line: &str) -> ParsedLine {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ParsedLine::Blank;
    }
    // Comments + the list header.
    if trimmed.starts_with('!') || (trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return ParsedLine::Comment;
    }
    // Cosmetic rules carry a `#…#` marker (and no path before it).
    if let Some(parsed) = parse_cosmetic(trimmed) {
        return parsed;
    }
    parse_network(trimmed)
}

/// Parse a cosmetic line, or `None` if there is no cosmetic marker (so the
/// caller falls through to network parsing).
fn parse_cosmetic(line: &str) -> Option<ParsedLine> {
    let hash = line.find('#')?;
    let (prefix, rest) = line.split_at(hash);
    // A `/` before the marker means this `#` is a URL fragment, not cosmetic.
    if prefix.contains('/') {
        return None;
    }
    let (is_exception, selector) = if let Some(sel) = rest.strip_prefix("#@#") {
        (true, sel)
    } else if let Some(sel) = rest.strip_prefix("##") {
        (false, sel)
    } else if is_unsupported_cosmetic_marker(rest) {
        return Some(ParsedLine::Unsupported(line.to_string()));
    } else {
        // A lone `#` that is not a cosmetic marker — not a cosmetic rule.
        return None;
    };
    let selector = selector.trim();
    if selector.is_empty() {
        return Some(ParsedLine::Unsupported(line.to_string()));
    }
    let (domains_include, domains_exclude) = parse_domain_list(prefix, ',');
    Some(ParsedLine::Cosmetic(CosmeticRule {
        selector: selector.to_string(),
        domains_include,
        domains_exclude,
        is_exception,
    }))
}

/// The uBlock/ABP extended cosmetic markers we recognise but do not implement
/// (procedural `:has()` hides, scriptlet + CSS injection, and their exceptions).
fn is_unsupported_cosmetic_marker(rest: &str) -> bool {
    const MARKERS: [&str; 6] = ["#?#", "#$#", "#%#", "#@?#", "#@$#", "#@%#"];
    MARKERS.iter().any(|m| rest.starts_with(m))
}

/// Parse a network filter line into a [`NetworkRule`] (or [`ParsedLine::Unsupported`]).
fn parse_network(line: &str) -> ParsedLine {
    let (is_exception, after_at) = line
        .strip_prefix("@@")
        .map_or((false, line), |rest| (true, rest));

    // Split the pattern body from its `$options` at the first `$`.
    let (body, opt_str) = after_at
        .split_once('$')
        .map_or((after_at, ""), |(b, o)| (b, o));

    // A `/regex/` body needs a regex engine we do not vendor — skip it honestly.
    if body.len() > 1 && body.starts_with('/') && body.ends_with('/') {
        return ParsedLine::Unsupported(line.to_string());
    }

    let Some(options) = parse_options(opt_str) else {
        return ParsedLine::Unsupported(line.to_string());
    };

    ParsedLine::Network(NetworkRule {
        raw: line.to_string(),
        pattern: Pattern::compile(body),
        is_exception,
        options,
    })
}

/// Parse the `$options` segment, or `None` if it carries an option we do not
/// model (so the whole rule is marked unsupported rather than mis-scoped).
fn parse_options(opt_str: &str) -> Option<RuleOptions> {
    let mut opts = RuleOptions::default();
    if opt_str.is_empty() {
        return Some(opts);
    }
    for raw_tok in opt_str.split(',') {
        let tok = raw_tok.trim();
        if tok.is_empty() {
            continue;
        }
        if let Some(domains) = tok.strip_prefix("domain=") {
            let (inc, exc) = parse_domain_list(domains, '|');
            opts.domains_include = inc;
            opts.domains_exclude = exc;
            continue;
        }
        let (negated, keyword) = tok
            .strip_prefix('~')
            .map_or((false, tok), |rest| (true, rest));
        match keyword {
            "third-party" | "3p" => opts.third_party = Some(!negated),
            // Recognised but not differentiated: matching is always
            // case-insensitive, so `match-case` is a documented no-op here.
            "match-case" => {}
            _ => {
                let Some(ty) = ResourceType::from_option(keyword) else {
                    // An option we do not model (`important`, `csp=`, `redirect=`,
                    // `popup`, `removeparam`, …) — refuse to half-apply the rule.
                    return None;
                };
                if negated {
                    opts.types.exclude(ty);
                } else {
                    opts.types.include(ty);
                }
            }
        }
    }
    Some(opts)
}

/// Split a `sep`-delimited domain list into (include, exclude) domains, honouring
/// the `~` exclusion prefix. Empty / whitespace entries are dropped.
fn parse_domain_list(list: &str, sep: char) -> (Vec<String>, Vec<String>) {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for entry in list.split(sep) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some(dom) = entry.strip_prefix('~') {
            if !dom.is_empty() {
                exclude.push(dom.to_ascii_lowercase());
            }
        } else {
            include.push(entry.to_ascii_lowercase());
        }
    }
    (include, exclude)
}

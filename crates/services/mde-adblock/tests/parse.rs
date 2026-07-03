//! BOOKMARKS-7 — filter-list **parser** fixtures.
//!
//! Asserts each `EasyList` line shape parses into the right typed rule (or is
//! honestly marked unsupported): network block / `@@` exception, cosmetic
//! `##` hide / `#@#` un-hide with domain scoping, `$options` (types,
//! `third-party`, `domain=`), comments/blanks, and the deliberately-skipped
//! shapes (regex bodies, scriptlet injects, unmodelled options).

use mde_adblock::{parse_line, CosmeticRule, FilterList, NetworkRule, ParsedLine, ResourceType};

fn network(line: &str) -> NetworkRule {
    let parsed = parse_line(line);
    assert!(
        matches!(parsed, ParsedLine::Network(_)),
        "expected a network rule for {line:?}, got {parsed:?}"
    );
    match parsed {
        ParsedLine::Network(r) => r,
        _ => unreachable!(),
    }
}

fn cosmetic(line: &str) -> CosmeticRule {
    let parsed = parse_line(line);
    assert!(
        matches!(parsed, ParsedLine::Cosmetic(_)),
        "expected a cosmetic rule for {line:?}, got {parsed:?}"
    );
    match parsed {
        ParsedLine::Cosmetic(c) => c,
        _ => unreachable!(),
    }
}

#[test]
fn network_block_and_exception() {
    assert!(!network("||doubleclick.net^").is_exception, "plain rule");
    assert!(
        network("@@||safe.example.com/asset.js").is_exception,
        "@@ marks an exception"
    );
}

#[test]
fn comments_and_blanks() {
    assert!(matches!(parse_line("! a comment"), ParsedLine::Comment));
    assert!(matches!(
        parse_line("[Adblock Plus 2.0]"),
        ParsedLine::Comment
    ));
    assert!(matches!(parse_line(""), ParsedLine::Blank));
    assert!(matches!(parse_line("   "), ParsedLine::Blank));
}

#[test]
fn cosmetic_generic_specific_and_exception() {
    let generic = cosmetic("##.ad-banner");
    assert_eq!(generic.selector, ".ad-banner");
    assert!(!generic.is_exception);
    assert!(generic.applies_to("anything.example"), "generic everywhere");

    let scoped = cosmetic("news.example,blog.example##.promo");
    assert_eq!(scoped.selector, ".promo");
    assert!(scoped.applies_to("news.example"));
    assert!(scoped.applies_to("sub.blog.example"), "subdomain scoped");
    assert!(!scoped.applies_to("other.example"), "not other sites");

    let unhide = cosmetic("news.example#@#.ad-banner");
    assert!(unhide.is_exception, "#@# is an un-hide exception");
    assert_eq!(unhide.selector, ".ad-banner");
}

#[test]
fn unsupported_shapes_are_marked_not_matched() {
    // A regex body — no regex engine is vendored.
    assert!(matches!(
        parse_line("/banner\\d+/"),
        ParsedLine::Unsupported(_)
    ));
    // A scriptlet / CSS-inject cosmetic marker.
    assert!(matches!(
        parse_line("example.com#$#abort-on-property-read foo"),
        ParsedLine::Unsupported(_)
    ));
    // An option we do not model — refuse to half-apply the rule.
    assert!(matches!(
        parse_line("||ads.example^$important"),
        ParsedLine::Unsupported(_)
    ));
    // A URL with a `#` fragment is NOT cosmetic (a `/` precedes the `#`).
    assert!(matches!(
        parse_line("||example.com/page#section"),
        ParsedLine::Network(_)
    ));
}

#[test]
fn options_parse_into_scope() {
    // A rule scoped to third-party scripts on publisher.com but not ads.publisher.com.
    let r = network("||cdn.example^$script,third-party,domain=publisher.com|~ads.publisher.com");
    // In-scope: a third-party script on publisher.com.
    assert!(r
        .options
        .applies(ResourceType::Script, "publisher.com", true));
    // Out of scope: an image (wrong type).
    assert!(!r
        .options
        .applies(ResourceType::Image, "publisher.com", true));
    // Out of scope: first-party (the rule wants third-party).
    assert!(!r
        .options
        .applies(ResourceType::Script, "publisher.com", false));
    // Out of scope: an excluded first-party domain.
    assert!(!r
        .options
        .applies(ResourceType::Script, "ads.publisher.com", true));
}

#[test]
fn whole_list_stats() {
    let text = "\
! Title: fixture
[Adblock Plus 2.0]
||doubleclick.net^
||ads.example^$script

##.ad
example.com##.promo
/regex.*/
||x.example^$important
";
    let list = FilterList::parse(text);
    assert_eq!(list.stats.network, 2, "two network rules");
    assert_eq!(list.stats.cosmetic, 2, "two cosmetic rules");
    assert_eq!(list.stats.comments, 2, "title + header");
    assert_eq!(list.stats.blank, 1);
    assert_eq!(list.stats.unsupported, 2, "regex + unmodelled option");
    assert_eq!(list.rule_count(), 4);
}

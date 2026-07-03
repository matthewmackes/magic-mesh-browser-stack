//! [`FilterList`] — a whole EasyList-format list parsed into its rules
//! (BOOKMARKS-7).
//!
//! Splits raw list text into lines, runs [`crate::parse_line`] over each, and
//! collects the network + cosmetic rules plus [`ParseStats`] (how many lines
//! were comments / blanks / unsupported). The engine ([`crate::Engine`]) folds
//! one or more [`FilterList`]s into its matching indexes.

use crate::rule::{parse_line, CosmeticRule, NetworkRule, ParsedLine};

/// A parsed filter list: its network + cosmetic rules and parse stats.
#[derive(Clone, Debug, Default)]
pub struct FilterList {
    /// The network block / allow rules, in list order.
    pub network: Vec<NetworkRule>,
    /// The cosmetic element-hide / un-hide rules, in list order.
    pub cosmetic: Vec<CosmeticRule>,
    /// Line-count statistics for the parse.
    pub stats: ParseStats,
}

/// Counts from parsing a filter list — for the honest "N rules, M skipped"
/// indicator the operator sees, and for tests asserting coverage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParseStats {
    /// Total lines read.
    pub total: usize,
    /// Network rules parsed.
    pub network: usize,
    /// Cosmetic rules parsed.
    pub cosmetic: usize,
    /// Comment + header lines.
    pub comments: usize,
    /// Blank lines.
    pub blank: usize,
    /// Recognised-but-unmodelled lines that were skipped (regex bodies,
    /// scriptlet injects, unmodelled options).
    pub unsupported: usize,
}

impl FilterList {
    /// Parse raw EasyList-format `text` (one rule per line) into a [`FilterList`].
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut list = Self::default();
        for line in text.lines() {
            list.stats.total += 1;
            match parse_line(line) {
                ParsedLine::Network(rule) => {
                    list.stats.network += 1;
                    list.network.push(rule);
                }
                ParsedLine::Cosmetic(rule) => {
                    list.stats.cosmetic += 1;
                    list.cosmetic.push(rule);
                }
                ParsedLine::Comment => list.stats.comments += 1,
                ParsedLine::Blank => list.stats.blank += 1,
                ParsedLine::Unsupported(_) => list.stats.unsupported += 1,
            }
        }
        list
    }

    /// The number of matchable rules (network + cosmetic) this list contributes.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.network.len() + self.cosmetic.len()
    }
}

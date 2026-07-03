//! [`Pattern`] — a compiled Adblock-Plus filter **match pattern** (BOOKMARKS-7).
//!
//! The URL-matching half of a network rule. A raw filter body such as
//! `||ads.example.com^`, `|http://start`, `/banner/*` or plain `doubleclick`
//! compiles to a token sequence + anchor flags, matched case-insensitively
//! against a request URL. The supported syntax is the common `EasyList` subset:
//!
//!   * `||` — **domain anchor**: match at a host label boundary (the start of
//!     the host or immediately after a `.` within the host).
//!   * `|` at the start / end — anchor the match to the URL start / end.
//!   * `^` — a **separator placeholder**: one character that is not part of a
//!     hostname/path token (per ABP: not a letter, digit, `_`, `-`, `.` or `%`),
//!     or the end of the URL.
//!   * `*` — a wildcard: any (possibly empty) run of characters.
//!
//! A `/regex/` filter body is **not** compiled (no regex engine is vendored); the
//! rule parser marks such lines unsupported so they are never mis-matched.

/// One token of a compiled [`Pattern`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// A literal byte run (already lowercased at compile time).
    Literal(Vec<u8>),
    /// The `^` separator placeholder.
    Separator,
    /// The `*` wildcard.
    Wildcard,
}

/// A compiled filter match pattern: a token sequence plus anchor flags.
///
/// Build one with [`Pattern::compile`], then test a URL with [`Pattern::matches`].
/// Matching is case-insensitive (the pattern literals and the URL are both
/// lowercased), matching ABP's default (the rare `$match-case` option aside).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    tokens: Vec<Token>,
    /// `|` at the start — the match must begin at URL offset 0.
    anchor_start: bool,
    /// `||` at the start — the match must begin at a host label boundary.
    anchor_host: bool,
    /// `|` at the end — the match must reach the end of the URL.
    anchor_end: bool,
}

/// Is `c` an ABP **separator** character — anything that is not a letter, digit,
/// `_`, `-`, `.` or `%`?
///
/// The `^` placeholder matches exactly one separator (or the end of the URL), so
/// `||example.com^` matches `example.com/`, `example.com:80` and a trailing
/// `example.com`, but not `example.company`.
#[must_use]
pub const fn is_separator(c: u8) -> bool {
    !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'%'))
}

impl Pattern {
    /// Compile a raw filter body (the part before any `$options`) into a
    /// [`Pattern`]. Anchors (`||`, `|`) and the `^`/`*` metacharacters are
    /// recognised; every other character is literal. The body is lowercased so
    /// matching is case-insensitive.
    #[must_use]
    pub fn compile(body: &str) -> Self {
        let mut s = body.to_ascii_lowercase();

        let anchor_host = s.starts_with("||");
        let anchor_start = !anchor_host && s.starts_with('|');
        if anchor_host {
            s.drain(..2);
        } else if anchor_start {
            s.drain(..1);
        }

        let anchor_end = s.ends_with('|');
        if anchor_end {
            s.pop();
        }

        let mut tokens = Vec::new();
        let mut lit: Vec<u8> = Vec::new();
        for &b in s.as_bytes() {
            match b {
                b'^' | b'*' => {
                    if !lit.is_empty() {
                        tokens.push(Token::Literal(std::mem::take(&mut lit)));
                    }
                    tokens.push(if b == b'^' {
                        Token::Separator
                    } else {
                        Token::Wildcard
                    });
                }
                _ => lit.push(b),
            }
        }
        if !lit.is_empty() {
            tokens.push(Token::Literal(lit));
        }

        Self {
            tokens,
            anchor_start,
            anchor_host,
            anchor_end,
        }
    }

    /// Does this pattern match `url`? `url` is the full request URL; it is
    /// lowercased here. `host_range` is the byte range of the host within `url`
    /// used only for the `||` domain anchor — pass `None` to skip host-anchored
    /// candidate positions (a `||` pattern then never matches, which is correct
    /// for a host-less URL).
    ///
    /// Hot callers that test many patterns against one URL should lowercase once
    /// and use [`Self::matches_lower`] to avoid re-allocating per rule.
    #[must_use]
    pub fn matches(&self, url: &str, host_range: Option<(usize, usize)>) -> bool {
        self.matches_lower(url.to_ascii_lowercase().as_bytes(), host_range)
    }

    /// Match against an already-ASCII-lowercased URL's bytes (the engine's hot
    /// path lowercases the request URL once and reuses it across every rule).
    /// `host_range` byte offsets are valid on the lowercased bytes because
    /// ASCII-lowercasing preserves length.
    #[must_use]
    pub fn matches_lower(&self, bytes: &[u8], host_range: Option<(usize, usize)>) -> bool {
        // Candidate start offsets depend on the leading anchor.
        if self.anchor_start {
            return self.match_from(bytes, 0);
        }
        if self.anchor_host {
            let Some((hs, he)) = host_range else {
                return false;
            };
            // Match at the host start or immediately after any `.` within it.
            if self.match_from(bytes, hs) {
                return true;
            }
            for i in hs..he {
                if bytes.get(i) == Some(&b'.') && self.match_from(bytes, i + 1) {
                    return true;
                }
            }
            return false;
        }
        // Unanchored: a substring match — try every start offset.
        (0..=bytes.len()).any(|start| self.match_from(bytes, start))
    }

    /// Match the token sequence against `bytes` starting at `pos`, honouring the
    /// end anchor. Wildcards backtrack; patterns are short so this stays cheap.
    fn match_from(&self, bytes: &[u8], pos: usize) -> bool {
        self.match_tokens(&self.tokens, bytes, pos)
    }

    fn match_tokens(&self, tokens: &[Token], bytes: &[u8], pos: usize) -> bool {
        let Some((tok, rest)) = tokens.split_first() else {
            // All tokens consumed: the end anchor (if any) requires end-of-URL.
            return !self.anchor_end || pos == bytes.len();
        };
        match tok {
            Token::Literal(lit) => {
                if bytes[pos..].starts_with(lit) {
                    self.match_tokens(rest, bytes, pos + lit.len())
                } else {
                    false
                }
            }
            Token::Separator => {
                if pos == bytes.len() {
                    // `^` also matches the end of the address (consumes nothing).
                    self.match_tokens(rest, bytes, pos)
                } else if is_separator(bytes[pos]) {
                    self.match_tokens(rest, bytes, pos + 1)
                } else {
                    false
                }
            }
            Token::Wildcard => {
                // Try consuming 0..=remaining characters, shortest first.
                (pos..=bytes.len()).any(|p| self.match_tokens(rest, bytes, p))
            }
        }
    }
}

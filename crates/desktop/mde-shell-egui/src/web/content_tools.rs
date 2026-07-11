//! Page content-tool request/result records — the small data carriers the Browser
//! surface builds when the operator invokes a content action on the active page:
//! spellcheck results, read-aloud, scrape-to-disk export, translate, and the
//! offline-cache capture bundle (viewport image + resources + PDF snapshot). Plain
//! value types (the only method is `BrowserSpellcheckResult`'s constructor/summary
//! helpers); `use super::*` pulls in the parent's `BrowserEngine`, `SpellMiss`,
//! `PathBuf`, `TextureHandle`. A pure relocation from the `web` god-module.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BrowserSpellcheckResult {
    pub(super) tab_index: usize,
    pub(super) misses: Vec<SpellMiss>,
    pub(super) error: Option<String>,
}

impl BrowserSpellcheckResult {
    pub(super) fn from_result(tab_index: usize, result: Result<Vec<SpellMiss>, String>) -> Self {
        match result {
            Ok(misses) => Self {
                tab_index,
                misses,
                error: None,
            },
            Err(error) => Self {
                tab_index,
                misses: Vec::new(),
                error: Some(error),
            },
        }
    }

    pub(super) fn is_visible(&self) -> bool {
        self.error.is_some() || !self.misses.is_empty()
    }

    pub(super) fn summary(&self) -> String {
        if let Some(error) = self.error.as_deref() {
            if error.trim().is_empty() {
                "Spellcheck unavailable".to_owned()
            } else {
                format!("Spellcheck unavailable: {error}")
            }
        } else {
            let count = self.misses.len();
            let plural = if count == 1 { "" } else { "s" };
            format!("{count} possible misspelling{plural}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadAloudRequest {
    pub(super) tab_index: usize,
    pub(super) engine: BrowserEngine,
    pub(super) url: String,
    pub(super) title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScrapeExportRequest {
    pub(super) tab_index: usize,
    pub(super) engine: BrowserEngine,
    pub(super) url: String,
    pub(super) title: String,
    pub(super) resources: Vec<mde_web_preview_client::ResourceRequestStatus>,
    pub(super) spool_dir: PathBuf,
    pub(super) dest_dir: PathBuf,
    pub(super) captured_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranslateRequest {
    pub(super) tab_index: usize,
    pub(super) engine: BrowserEngine,
    pub(super) url: String,
    pub(super) title: String,
    pub(super) source_lang: String,
    pub(super) target_lang: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OfflineCacheRequest {
    pub(super) tab_index: usize,
    pub(super) engine: BrowserEngine,
    pub(super) url: String,
    pub(super) title: String,
    pub(super) viewport: Option<OfflineCacheViewportImage>,
    pub(super) resources: Vec<OfflineCacheResource>,
    pub(super) pdf_snapshot: Option<OfflineCachePdf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OfflineCacheArchive {
    pub(super) mime: String,
    pub(super) filename: String,
    pub(super) bytes: usize,
    pub(super) data_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OfflineCachePdf {
    pub(super) mime: String,
    pub(super) filename: String,
    pub(super) bytes: usize,
    pub(super) data_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OfflineCacheResource {
    pub(super) url: String,
    pub(super) resource: String,
    pub(super) allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OfflineCacheViewportImage {
    pub(super) mime: String,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) data_base64: String,
}

#[derive(Clone)]
pub(super) struct OfflineCacheViewportTexture {
    pub(super) data_sig: u64,
    pub(super) texture: Option<TextureHandle>,
}

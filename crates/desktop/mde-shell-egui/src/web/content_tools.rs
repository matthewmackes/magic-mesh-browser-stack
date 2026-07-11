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

/// Spellcheck actions on the Browser surface state — requesting a page-text check,
/// draining the async result onto the tab, and applying a suggested correction —
/// kept beside the [`BrowserSpellcheckResult`] carrier they populate. `use super::*`
/// pulls in the parent's `spellcheck_highlight_words` / `spellcheck_notice` helpers
/// and `WebState`'s private fields. A pure relocation from the `web` god-module.
impl WebState {
    pub(super) fn apply_spellcheck_correction(
        &mut self,
        tab_index: usize,
        word: &str,
        replacement: &str,
    ) {
        self.apply_spellcheck_correction_inner(tab_index, word, replacement, false);
    }

    pub(super) fn apply_spellcheck_correction_all(
        &mut self,
        tab_index: usize,
        word: &str,
        replacement: &str,
    ) {
        self.apply_spellcheck_correction_inner(tab_index, word, replacement, true);
    }

    pub(super) fn apply_spellcheck_correction_at(
        &mut self,
        tab_index: usize,
        word: &str,
        replacement: &str,
        occurrence: u16,
    ) {
        let word = word.trim();
        let replacement = replacement.trim();
        if word.is_empty() || replacement.is_empty() {
            return;
        }
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            self.capture_notice = Some("Spelling correction unavailable: tab closed".to_owned());
            return;
        };
        if tab.session.is_crashed() {
            self.capture_notice = Some("Spelling correction unavailable: page crashed".to_owned());
            return;
        }
        tab.session.apply_spellcheck_correction_at(
            word.to_owned(),
            replacement.to_owned(),
            occurrence,
        );
        self.capture_notice = Some(format!(
            "Spelling: replaced occurrence {} of {word} with {replacement}",
            u32::from(occurrence) + 1
        ));
    }

    fn apply_spellcheck_correction_inner(
        &mut self,
        tab_index: usize,
        word: &str,
        replacement: &str,
        replace_all: bool,
    ) {
        let word = word.trim();
        let replacement = replacement.trim();
        if word.is_empty() || replacement.is_empty() {
            return;
        }
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            self.capture_notice = Some("Spelling correction unavailable: tab closed".to_owned());
            return;
        };
        if tab.session.is_crashed() {
            self.capture_notice = Some("Spelling correction unavailable: page crashed".to_owned());
            return;
        }
        if replace_all {
            tab.session
                .apply_spellcheck_correction_all(word.to_owned(), replacement.to_owned());
            self.capture_notice = Some(format!("Spelling: replaced all {word} with {replacement}"));
        } else {
            tab.session
                .apply_spellcheck_correction(word.to_owned(), replacement.to_owned());
            self.capture_notice = Some(format!("Spelling: replaced {word} with {replacement}"));
        }
    }

    pub(super) fn request_active_spellcheck(&mut self) {
        if !self.can_drive_page_tools() {
            self.capture_notice = Some("Spelling unavailable: no live page".to_owned());
            return;
        }
        if self.spellcheck.in_flight.is_some() {
            self.capture_notice = Some("Spelling: check already running".to_owned());
            return;
        }
        let id = self.next_page_text_request_id;
        self.next_page_text_request_id = self.next_page_text_request_id.saturating_add(1).max(1);
        let active = self.active;
        if let Some(tab) = self.active_tab() {
            tab.session.request_page_text(id, 64 * 1024);
            self.pending_spell_requests.insert(id, active);
            self.capture_notice = Some("Spelling: reading page text".to_owned());
        }
    }

    pub(super) fn poll_spellcheck(&mut self) {
        let Some((id, tab_index, result)) = self.spellcheck.poll() else {
            return;
        };
        if self.pending_spell_requests.contains_key(&id) {
            return;
        }
        let highlight_words = match &result {
            Ok(misses) => spellcheck_highlight_words(misses),
            Err(_) => Vec::new(),
        };
        if let Some(tab) = self.tabs.get_mut(tab_index) {
            if !tab.session.is_crashed() {
                tab.session.set_spellcheck_highlights(highlight_words);
            }
        }
        self.latest_spellcheck = Some(BrowserSpellcheckResult::from_result(
            tab_index,
            result.clone(),
        ));
        self.capture_notice = Some(spellcheck_notice(result));
    }
}

/// Read-aloud action on the Browser surface state — snapshots the active tab into
/// a [`ReadAloudRequest`] and asks the helper for the page text (the TTS handoff
/// happens later in the parent's shared page-text handler). A pure relocation from
/// the `web` god-module.
impl WebState {
    pub(super) fn request_active_read_aloud(&mut self) {
        if !self.can_drive_page_tools() {
            self.capture_notice = Some("Read aloud unavailable: no live page".to_owned());
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            self.capture_notice = Some("Read aloud unavailable: no live page".to_owned());
            return;
        };
        let request = ReadAloudRequest {
            tab_index: self.active,
            engine: tab.engine,
            url: tab.session.nav().url.clone(),
            title: tab.session.title().to_owned(),
        };
        let id = self.next_page_text_request_id;
        self.next_page_text_request_id = self.next_page_text_request_id.saturating_add(1).max(1);
        if let Some(tab) = self.active_tab() {
            tab.session.request_page_text(id, 64 * 1024);
            self.pending_read_aloud_requests.insert(id, request);
            self.capture_notice = Some("Read aloud: reading page text".to_owned());
        }
    }
}

/// Translate-page action on the Browser surface state — snapshots the active tab
/// into a [`TranslateRequest`] and asks the helper for the page text (the private
/// translation handoff happens later in the parent's shared page-text handler).
/// A pure relocation from the `web` god-module.
impl WebState {
    pub(super) fn request_active_translate_page(&mut self) {
        if !self.can_drive_page_tools() {
            self.capture_notice = Some("Translate unavailable: no live page".to_owned());
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            self.capture_notice = Some("Translate unavailable: no live page".to_owned());
            return;
        };
        let request = TranslateRequest {
            tab_index: self.active,
            engine: tab.engine,
            url: tab.session.nav().url.clone(),
            title: tab.session.title().to_owned(),
            source_lang: "auto".to_owned(),
            target_lang: browser_translate_target_lang(),
        };
        let id = self.next_page_text_request_id;
        self.next_page_text_request_id = self.next_page_text_request_id.saturating_add(1).max(1);
        if let Some(tab) = self.active_tab() {
            tab.session.request_page_text(id, 64 * 1024);
            self.pending_translate_requests.insert(id, request);
            self.capture_notice = Some("Translate: reading page text".to_owned());
        }
    }
}

/// Offline-cache actions on the Browser surface state — requesting an active-page
/// snapshot, polling/applying the helper's result off the bus, indexing it by URL
/// for the gated-page fallback, and saving the captured PDF/MHTML to disk — kept
/// beside the [`OfflineCacheRequest`] carrier and the offline-cache value types.
/// The result type ([`BrowserOfflineCacheResult`]) and the parse/body/bytes bus
/// helpers stay in the parent and arrive via `use super::*`. A pure relocation
/// from the `web` god-module.
impl WebState {
    pub(super) fn poll_offline_cache_results(&mut self) {
        if self
            .offline_cache_result_last_poll
            .is_some_and(|last| last.elapsed() < OFFLINE_CACHE_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.offline_cache_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_offline_cache_result_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.offline_cache_result_cursor.as_deref())
        else {
            return;
        };
        for msg in msgs {
            self.offline_cache_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(result) = parse_offline_cache_result(body) else {
                continue;
            };
            self.apply_offline_cache_result(result);
        }
    }

    pub(super) fn apply_offline_cache_result(&mut self, result: BrowserOfflineCacheResult) {
        if result.host != local_hostname() {
            return;
        }
        let chars = result.text.chars().count();
        self.capture_notice = Some(format!(
            "Offline cache ready: {} character{}",
            chars,
            plural(chars)
        ));
        for key in cache_url_keys(&result.url) {
            self.offline_cache_by_url.insert(key, result.clone());
        }
        self.latest_offline_cache = Some(result);
    }

    pub(super) fn cached_snapshot_for_url(&self, url: &str) -> Option<&BrowserOfflineCacheResult> {
        cache_url_keys(url)
            .into_iter()
            .find_map(|key| self.offline_cache_by_url.get(&key))
    }

    pub(super) fn offline_cache_fallback_for_unavailable(
        &self,
    ) -> Option<&BrowserOfflineCacheResult> {
        match self.tabs.get(self.active) {
            Some(tab) if tab.session.is_crashed() => {
                self.cached_snapshot_for_url(tab.session.nav().url.trim())
            }
            None => self.cached_snapshot_for_url(self.address.trim()),
            _ => None,
        }
    }

    pub(super) fn open_latest_offline_cache_pdf(&mut self) {
        match self.save_latest_offline_cache_pdf_to_dir(browser_pdf_dir()) {
            Ok(path) => {
                if let Some(result) = &self.latest_offline_cache {
                    self.last_saved_pdf = Some(SavedPdf {
                        path,
                        url: result.url.clone(),
                        title: result.title.clone(),
                    });
                }
                self.open_last_saved_pdf();
            }
            Err(err) => {
                self.capture_notice = Some(format!("Offline PDF failed: {err}"));
            }
        }
    }

    pub(super) fn save_latest_offline_cache_pdf_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let result = self
            .latest_offline_cache
            .as_ref()
            .ok_or_else(|| "no offline copy".to_owned())?;
        let pdf = result
            .pdf_snapshot
            .as_ref()
            .ok_or_else(|| "offline copy has no PDF snapshot".to_owned())?;
        let bytes = offline_cache_pdf_bytes(pdf)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let path = dir.join(&pdf.filename);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    pub(super) fn save_latest_offline_cache_archive(&mut self) {
        match self.save_latest_offline_cache_archive_to_dir(browser_capture_dir()) {
            Ok(path) => self.record_capture_success("Saved offline archive", &path),
            Err(err) => {
                self.capture_notice = Some(format!("Offline archive failed: {err}"));
            }
        }
    }

    pub(super) fn save_latest_offline_cache_archive_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let result = self
            .latest_offline_cache
            .as_ref()
            .ok_or_else(|| "no offline copy".to_owned())?;
        let archive = result
            .archive_mhtml
            .as_ref()
            .ok_or_else(|| "offline copy has no MHTML archive".to_owned())?;
        let bytes = offline_cache_archive_bytes(archive)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let path = dir.join(&archive.filename);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    pub(super) fn request_active_offline_cache(&mut self) {
        if !self.can_drive_page_tools() {
            self.capture_notice = Some("Offline cache unavailable: no live page".to_owned());
            return;
        }
        let (engine, url, title, viewport, resources) = {
            let Some(tab) = self.tabs.get(self.active) else {
                self.capture_notice = Some("Offline cache unavailable: no live page".to_owned());
                return;
            };
            (
                tab.engine,
                tab.session.nav().url.clone(),
                tab.session.title().to_owned(),
                tab.last_frame
                    .as_ref()
                    .and_then(offline_cache_viewport_image),
                offline_cache_resource_manifest(&tab.session.recent_resource_requests()),
            )
        };
        let pdf_snapshot = self
            .last_saved_pdf
            .as_ref()
            .filter(|saved| saved.url == url)
            .and_then(offline_cache_pdf_snapshot);
        let request = OfflineCacheRequest {
            tab_index: self.active,
            engine,
            url,
            title,
            viewport,
            resources,
            pdf_snapshot,
        };
        let id = self.next_page_text_request_id;
        self.next_page_text_request_id = self.next_page_text_request_id.saturating_add(1).max(1);
        if let Some(tab) = self.active_tab() {
            tab.session.request_page_text(id, 64 * 1024);
            self.pending_offline_cache_requests.insert(id, request);
            self.capture_notice = Some("Offline cache: reading page text".to_owned());
        }
    }
}

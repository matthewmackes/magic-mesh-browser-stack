//! The Browser surface's **capture, scrape, and media-export helpers** — the pure,
//! non-UI functions that turn a live page into on-disk artifacts. No egui `Ui`
//! rendering lives here; these are the headless, unit-asserted building blocks the
//! `WebState` capture/scrape/export actions (and the sibling `content_tools` /
//! `printing` modules) call to produce files and byte payloads.
//!
//! Four cohesive concerns, all headless:
//!  * **Output locations + filenames** — the `browser_*_dir` spool roots and the
//!    `*_filename_for` builders (screenshots, full-page, MHTML, annotated / callout
//!    / freehand overlays, region crops, scrape exports, media manifests, PDFs) plus
//!    the shared `output_filename_for` slugger and `file_url_for_path`.
//!  * **Page scrape + media extraction** — `active_page_scrape_documents` and its
//!    crawl-seed / crawl-manifest / text-extract / DOM-extract pipeline (CSV, Markdown,
//!    JSON), the same-origin gate, and the `active_page_media_manifest` /
//!    `active_page_media_asset_requests` media sniffers with `MediaAssetSelection`.
//!  * **Archive documents** — the MHTML capture / offline-cache document builders,
//!    HTML/header escaping, and the PDF readability probe.
//!  * **Image plumbing** — PNG encoding, the annotate (caption / callout / freehand)
//!    overlays, the low-level pixel drawing + tiny-glyph rasteriser, and the
//!    `PixelRegion` crop math.
//!
//! `use super::*` pulls in the parent's engine/state types, the `TransferJob` /
//! `ResourceRequestStatus` records, the bus `publish*`/`unix_ms`/`host_of` helpers,
//! and the `std`/`serde_json`/`egui` re-exports. A pure relocation from the `web`
//! god-module — no behaviour change.

use super::*;

pub(super) fn browser_capture_dir() -> PathBuf {
    std::env::var_os("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("{} Captures", browser_product_label()))
}

pub(super) fn browser_pdf_dir() -> PathBuf {
    std::env::var_os("XDG_DOCUMENTS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Documents")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("{} PDFs", browser_product_label()))
}

pub(super) fn browser_print_spool_dir() -> PathBuf {
    std::env::temp_dir().join("mde-browser-cups")
}

pub(super) fn browser_scrape_spool_dir() -> PathBuf {
    std::env::temp_dir().join("mde-browser-scrapes")
}

pub(super) fn browser_media_spool_dir() -> PathBuf {
    std::env::temp_dir().join("mde-browser-media")
}

pub(super) fn capture_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser", "png", url, title, unix_ms)
}

pub(super) fn capture_full_page_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-full-page", "png", url, title, unix_ms)
}

pub(super) fn capture_mhtml_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser", "mhtml", url, title, unix_ms)
}

pub(super) fn capture_annotated_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-annotated", "png", url, title, unix_ms)
}

pub(super) fn capture_callout_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-callout", "png", url, title, unix_ms)
}

pub(super) fn capture_freehand_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-freehand", "png", url, title, unix_ms)
}

pub(super) fn scrape_export_filename_for(
    url: &str,
    title: &str,
    unix_ms: u64,
    ext: &str,
) -> String {
    output_filename_for("mde-browser-scrape", ext, url, title, unix_ms)
}

pub(super) fn media_manifest_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-media-manifest", "json", url, title, unix_ms)
}

pub(super) fn media_asset_request_filename_for(
    page_url: &str,
    title: &str,
    asset_url: &str,
    unix_ms: u64,
    index: usize,
) -> String {
    let base = output_filename_for(
        "mde-browser-media-download",
        "json",
        page_url,
        title,
        unix_ms,
    );
    let stem = base.strip_suffix(".json").unwrap_or(&base);
    let hint = sanitize_filename_component(&media_filename_hint(asset_url), 48);
    format!("{stem}-{index:03}-{hint}.download.json")
}

pub(super) fn capture_region_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-region", "png", url, title, unix_ms)
}

pub(super) fn pdf_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser", "pdf", url, title, unix_ms)
}

pub(super) fn print_pdf_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-print", "pdf", url, title, unix_ms)
}

pub(super) fn pdf_file_looks_readable(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    use std::io::Read;
    file.read_exact(&mut magic).is_ok() && magic == *b"%PDF"
}

pub(super) fn file_url_for_path(path: &Path) -> Result<String, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| format!("could not resolve current directory: {err}"))?
            .join(path)
    };
    let text = path.to_string_lossy();
    let mut out = String::from("file://");
    for byte in text.as_bytes() {
        match *byte {
            b'/' => out.push('/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*byte));
            }
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    Ok(out)
}

fn output_filename_for(prefix: &str, ext: &str, url: &str, title: &str, unix_ms: u64) -> String {
    let seed = host_of(url)
        .or_else(|| {
            let title = title.trim();
            (!title.is_empty()).then(|| title.to_owned())
        })
        .unwrap_or_else(|| "page".to_owned());
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in seed.chars() {
        let out = if ch.is_ascii_alphanumeric() {
            last_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if !last_dash {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = out {
            slug.push(ch);
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "page" } else { slug };
    format!("{prefix}-{unix_ms}-{slug}.{ext}")
}

pub(super) fn active_page_scrape_documents(
    url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
    page_text: Option<&str>,
    page_scrape_body: Option<&str>,
) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let label = if title.trim().is_empty() {
        host_of(url).unwrap_or_else(|| "Untitled page".to_owned())
    } else {
        title.trim().to_owned()
    };
    let crawl_seed = active_page_scrape_crawl_seed(url, recent);
    let dom_extract = scrape_dom_extract(url, page_scrape_body)?;
    let crawl_manifest = active_page_scrape_crawl_manifest(url, &crawl_seed, &dom_extract.links);
    let text_extract = if let Some(text) = dom_extract.text.as_deref() {
        scrape_text_extract_with_truncated(text, dom_extract.text_truncated)
    } else {
        scrape_text_extract(page_text)
    };
    let mut json_value = serde_json::json!({
        "op": "browser_active_page_scrape",
        "scope": "active_page_metadata_with_crawl_seed_text_and_dom",
        "url": url,
        "title": label,
        "engine": engine.wire(),
        "captured_ms": unix_ms,
        "formats": ["json", "csv", "md"],
        "crawl_seed_count": crawl_seed.len(),
        "crawl_manifest_status": if crawl_manifest.is_empty() { "empty" } else { "ready" },
        "crawl_execution_status": "not_started",
        "crawl_manifest_max_depth": 1,
        "crawl_manifest_count": crawl_manifest.len(),
        "crawl_seed": crawl_seed
            .iter()
            .map(|seed| {
                serde_json::json!({
                    "url": seed.url,
                    "resource": seed.resource,
                    "allowed": seed.allowed,
                    "same_origin": true,
                })
            })
            .collect::<Vec<_>>(),
        "crawl_manifest": crawl_manifest
            .iter()
            .map(|target| {
                serde_json::json!({
                    "url": target.url,
                    "source": target.source,
                    "resource": target.resource,
                    "allowed": target.allowed,
                    "same_origin": true,
                    "depth": target.depth,
                })
            })
            .collect::<Vec<_>>(),
        "extracted_text_status": text_extract.status,
        "extracted_text_chars": text_extract.original_chars,
        "extracted_text_truncated": text_extract.truncated,
        "dom_extract_status": dom_extract.status,
        "article_extract_status": dom_extract.article_status,
        "article_text_chars": dom_extract.article_text_chars,
        "article_text_truncated": dom_extract.article_text_truncated,
        "article_selector": dom_extract.article_selector,
        "canonical_url": dom_extract.canonical_url,
        "meta_description": dom_extract.meta_description,
        "document_lang": dom_extract.document_lang,
        "dom_link_count": dom_extract.links.len(),
        "dom_heading_count": dom_extract.headings.len(),
        "dom_links": dom_extract.links
            .iter()
            .map(|link| {
                serde_json::json!({
                    "url": link.url,
                    "text": link.text,
                    "rel": link.rel,
                    "target": link.target,
                    "same_origin": link.same_origin,
                })
            })
            .collect::<Vec<_>>(),
        "dom_headings": dom_extract.headings
            .iter()
            .map(|heading| {
                serde_json::json!({
                    "level": heading.level,
                    "text": heading.text,
                })
            })
            .collect::<Vec<_>>(),
    });
    if let Some(text) = &text_extract.text {
        json_value["extracted_text"] = serde_json::Value::String(text.clone());
    }
    if let Some(text) = &dom_extract.article_text {
        json_value["article_text"] = serde_json::Value::String(text.clone());
    }
    let json = serde_json::to_vec_pretty(&json_value)
        .map_err(|err| format!("encode scrape JSON: {err}"))?;
    let mut csv = format!(
        "captured_ms,engine,title,url,scope,seed_url,seed_resource,seed_allowed,text_status,text_chars,text_truncated,text,dom_kind,dom_url,dom_text,dom_level,dom_same_origin,dom_rel,dom_target\n{},{},{},{},active_page_metadata_with_crawl_seed_text_and_dom,,,,{},{},{},{},,,,,,,\n",
        unix_ms,
        csv_cell(engine.wire()),
        csv_cell(&label),
        csv_cell(url),
        csv_cell(text_extract.status),
        text_extract.original_chars,
        text_extract.truncated,
        csv_cell(text_extract.text.as_deref().unwrap_or(""))
    );
    for seed in &crawl_seed {
        csv.push_str(&format!(
            "{},{},{},{},crawl_seed,{},{},{},,,,,,,,,,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&seed.url),
            csv_cell(seed.resource),
            seed.allowed
        ));
    }
    for target in &crawl_manifest {
        csv.push_str(&format!(
            "{},{},{},{},crawl_manifest,{},{},{},,,,crawl_target,{},{},{},true,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&target.url),
            csv_cell(target.source),
            target.allowed,
            csv_cell(&target.url),
            csv_cell(target.resource),
            target.depth
        ));
    }
    for link in &dom_extract.links {
        csv.push_str(&format!(
            "{},{},{},{},dom_link,,,,,,,,link,{},{},,{},{},{}\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&link.url),
            csv_cell(&link.text),
            link.same_origin,
            csv_cell(&link.rel),
            csv_cell(&link.target)
        ));
    }
    for heading in &dom_extract.headings {
        csv.push_str(&format!(
            "{},{},{},{},dom_heading,,,,,,,,heading,,{},{},,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&heading.text),
            heading.level
        ));
    }
    if let Some(article_text) = &dom_extract.article_text {
        csv.push_str(&format!(
            "{},{},{},{},dom_article,,,,,,,,article,,{},,{},{},{}\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(article_text),
            false,
            csv_cell(&dom_extract.article_selector),
            csv_cell(dom_extract.article_status)
        ));
    }
    if !dom_extract.canonical_url.is_empty() {
        csv.push_str(&format!(
            "{},{},{},{},dom_canonical,,,,,,,,canonical,{},canonical,,{},,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&dom_extract.canonical_url),
            scrape_url_same_origin(url, &dom_extract.canonical_url)
        ));
    }
    if !dom_extract.meta_description.is_empty() {
        csv.push_str(&format!(
            "{},{},{},{},dom_meta_description,,,,,,,,meta_description,,{},,,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&dom_extract.meta_description)
        ));
    }
    if !dom_extract.document_lang.is_empty() {
        csv.push_str(&format!(
            "{},{},{},{},dom_document_lang,,,,,,,,document_lang,,{},,,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&dom_extract.document_lang)
        ));
    }
    let csv = csv.into_bytes();
    let seed_md = if crawl_seed.is_empty() {
        "No same-origin crawl seed URLs were observed for this page.".to_owned()
    } else {
        crawl_seed
            .iter()
            .map(|seed| {
                format!(
                    "- `{}` ({}, allowed={})",
                    seed.url.replace('`', "\\`"),
                    seed.resource,
                    seed.allowed
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text_md = match &text_extract.text {
        Some(text) if !text.is_empty() => {
            format!("```text\n{}\n```", text.replace("```", "`\\`\\`"))
        }
        Some(_) => "No visible page text was available for this page.".to_owned(),
        None => "Visible page text was not requested for this export path.".to_owned(),
    };
    let crawl_manifest_md = if crawl_manifest.is_empty() {
        "No same-origin crawl targets were available for this export.".to_owned()
    } else {
        crawl_manifest
            .iter()
            .map(|target| {
                format!(
                    "- `{}` (source={}, resource={}, depth={}, allowed={})",
                    target.url.replace('`', "\\`"),
                    target.source,
                    target.resource,
                    target.depth,
                    target.allowed
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let links_md = if dom_extract.links.is_empty() {
        match dom_extract.status {
            "not_requested" => "DOM links were not requested for this export path.".to_owned(),
            _ => "No DOM links were available for this page.".to_owned(),
        }
    } else {
        dom_extract
            .links
            .iter()
            .map(|link| {
                format!(
                    "- [{}]({}) (same_origin={}, rel=`{}`, target=`{}`)",
                    markdown_inline_text(&link.text),
                    link.url,
                    link.same_origin,
                    link.rel.replace('`', "\\`"),
                    link.target.replace('`', "\\`")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let headings_md = if dom_extract.headings.is_empty() {
        match dom_extract.status {
            "not_requested" => "DOM headings were not requested for this export path.".to_owned(),
            _ => "No DOM headings were available for this page.".to_owned(),
        }
    } else {
        dom_extract
            .headings
            .iter()
            .map(|heading| {
                format!(
                    "- h{} {}",
                    heading.level,
                    markdown_inline_text(&heading.text)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let article_md = match &dom_extract.article_text {
        Some(text) if !text.is_empty() => {
            let mut lines = vec![format!(
                "- Status: `{}`, selector `{}`, chars `{}`, truncated `{}`",
                dom_extract.article_status,
                dom_extract.article_selector.replace('`', "\\`"),
                dom_extract.article_text_chars,
                dom_extract.article_text_truncated
            )];
            if !dom_extract.canonical_url.is_empty() {
                lines.push(format!(
                    "- Canonical: `{}`",
                    dom_extract.canonical_url.replace('`', "\\`")
                ));
            }
            if !dom_extract.meta_description.is_empty() {
                lines.push(format!(
                    "- Description: {}",
                    markdown_inline_text(&dom_extract.meta_description)
                ));
            }
            if !dom_extract.document_lang.is_empty() {
                lines.push(format!(
                    "- Language: `{}`",
                    dom_extract.document_lang.replace('`', "\\`")
                ));
            }
            lines.push(String::new());
            lines.push("```text".to_owned());
            lines.push(text.replace("```", "`\\`\\`"));
            lines.push("```".to_owned());
            lines.join("\n")
        }
        Some(_) => "No article/main-body text was available for this page.".to_owned(),
        None => match dom_extract.status {
            "not_requested" => {
                "Article/main-body extraction was not requested for this export path.".to_owned()
            }
            _ => "No article/main-body text was available for this page.".to_owned(),
        },
    };
    let md = format!(
        "# {}\n\n- URL: `{}`\n- Engine: `{}`\n- Captured: `{}`\n- Scope: active page metadata with bounded crawl seed, extracted text, DOM links/headings/article metadata, and crawl manifest\n- Crawl seed URLs: `{}`\n- Crawl manifest URLs: `{}` depth-1 same-origin targets, execution `not_started`\n- Extracted text: `{}` chars, status `{}`, truncated `{}`\n- DOM extract: status `{}`, links `{}`, headings `{}`\n- Article extract: status `{}`, chars `{}`, truncated `{}`\n\n## Extracted Text\n\n{}\n\n## Article Extract\n\n{}\n\n## DOM Links\n\n{}\n\n## DOM Headings\n\n{}\n\n## Crawl Manifest\n\n{}\n\n## Crawl Seed\n\n{}\n\nThis export records bounded same-origin crawl targets and does not recursively fetch them.\n",
        markdown_heading_text(&label),
        url.replace('`', "\\`"),
        browser_engine_export_label(engine),
        unix_ms,
        crawl_seed.len(),
        crawl_manifest.len(),
        text_extract.original_chars,
        text_extract.status,
        text_extract.truncated,
        dom_extract.status,
        dom_extract.links.len(),
        dom_extract.headings.len(),
        dom_extract.article_status,
        dom_extract.article_text_chars,
        dom_extract.article_text_truncated,
        text_md,
        article_md,
        links_md,
        headings_md,
        crawl_manifest_md,
        seed_md
    )
    .into_bytes();
    Ok(vec![("json", json), ("csv", csv), ("md", md)])
}

fn browser_engine_export_label(engine: BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Cef => "Chromium",
        BrowserEngine::Servo => "Lightweight",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeCrawlSeed {
    url: String,
    resource: &'static str,
    allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeCrawlTarget {
    url: String,
    source: &'static str,
    resource: &'static str,
    allowed: bool,
    depth: u8,
}

fn active_page_scrape_crawl_seed(
    page_url: &str,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Vec<ScrapeCrawlSeed> {
    let Ok(page) = reqwest::Url::parse(page_url) else {
        return Vec::new();
    };
    let Some(origin_host) = page.host_str().map(str::to_ascii_lowercase) else {
        return Vec::new();
    };
    let origin_scheme = page.scheme().to_ascii_lowercase();
    let origin_port = page.port_or_known_default();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for resource in recent.iter().rev() {
        if out.len() >= SCRAPE_CRAWL_SEED_MAX_COUNT {
            break;
        }
        let url = resource.url.trim();
        if url.is_empty() {
            continue;
        }
        let Ok(parsed) = reqwest::Url::parse(url) else {
            continue;
        };
        if parsed.scheme().to_ascii_lowercase() != origin_scheme
            || parsed.host_str().map(str::to_ascii_lowercase) != Some(origin_host.clone())
            || parsed.port_or_known_default() != origin_port
        {
            continue;
        }
        let normalized = parsed.to_string();
        if !seen.insert(normalized.clone()) {
            continue;
        }
        out.push(ScrapeCrawlSeed {
            url: clamp_chars(&normalized, MEDIA_SNIFFER_URL_MAX_CHARS),
            resource: offline_cache_resource_type_name(resource.resource),
            allowed: resource.allowed,
        });
    }
    out.reverse();
    out
}

fn active_page_scrape_crawl_manifest(
    page_url: &str,
    crawl_seed: &[ScrapeCrawlSeed],
    dom_links: &[ScrapeDomLink],
) -> Vec<ScrapeCrawlTarget> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for seed in crawl_seed {
        if out.len() >= SCRAPE_CRAWL_MANIFEST_MAX_COUNT {
            break;
        }
        if !seen.insert(seed.url.clone()) {
            continue;
        }
        out.push(ScrapeCrawlTarget {
            url: seed.url.clone(),
            source: "telemetry",
            resource: seed.resource,
            allowed: seed.allowed,
            depth: 1,
        });
    }
    for link in dom_links {
        if out.len() >= SCRAPE_CRAWL_MANIFEST_MAX_COUNT {
            break;
        }
        if !link.same_origin || !scrape_url_same_origin(page_url, &link.url) {
            continue;
        }
        if !seen.insert(link.url.clone()) {
            continue;
        }
        out.push(ScrapeCrawlTarget {
            url: link.url.clone(),
            source: "dom_link",
            resource: "document",
            allowed: true,
            depth: 1,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeTextExtract {
    status: &'static str,
    text: Option<String>,
    original_chars: usize,
    truncated: bool,
}

fn scrape_text_extract(page_text: Option<&str>) -> ScrapeTextExtract {
    if let Some(text) = page_text {
        scrape_text_extract_with_truncated(text, false)
    } else {
        ScrapeTextExtract {
            status: "not_requested",
            text: None,
            original_chars: 0,
            truncated: false,
        }
    }
}

fn scrape_text_extract_with_truncated(text: &str, helper_truncated: bool) -> ScrapeTextExtract {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, SCRAPE_EXTRACT_TEXT_MAX_CHARS);
    ScrapeTextExtract {
        status: if text.is_empty() {
            "no_text"
        } else {
            "captured"
        },
        text: Some(text),
        original_chars,
        truncated: helper_truncated || original_chars > SCRAPE_EXTRACT_TEXT_MAX_CHARS,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeDomExtract {
    status: &'static str,
    text: Option<String>,
    text_truncated: bool,
    article_status: &'static str,
    article_text: Option<String>,
    article_text_chars: usize,
    article_text_truncated: bool,
    article_selector: String,
    canonical_url: String,
    meta_description: String,
    document_lang: String,
    links: Vec<ScrapeDomLink>,
    headings: Vec<ScrapeDomHeading>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeDomLink {
    url: String,
    text: String,
    rel: String,
    target: String,
    same_origin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeDomHeading {
    level: u8,
    text: String,
}

fn scrape_dom_extract(page_url: &str, body: Option<&str>) -> Result<ScrapeDomExtract, String> {
    let Some(body) = body else {
        return Ok(ScrapeDomExtract {
            status: "not_requested",
            text: None,
            text_truncated: false,
            article_status: "not_requested",
            article_text: None,
            article_text_chars: 0,
            article_text_truncated: false,
            article_selector: String::new(),
            canonical_url: String::new(),
            meta_description: String::new(),
            document_lang: String::new(),
            links: Vec::new(),
            headings: Vec::new(),
        });
    };
    if body.trim().is_empty() {
        return Ok(ScrapeDomExtract {
            status: "empty",
            text: Some(String::new()),
            text_truncated: false,
            article_status: "empty",
            article_text: Some(String::new()),
            article_text_chars: 0,
            article_text_truncated: false,
            article_selector: String::new(),
            canonical_url: String::new(),
            meta_description: String::new(),
            document_lang: String::new(),
            links: Vec::new(),
            headings: Vec::new(),
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("decode scrape DOM JSON: {err}"))?;
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(|text| clamp_chars(text.trim(), SCRAPE_EXTRACT_TEXT_MAX_CHARS))
        .unwrap_or_default();
    let text_truncated = value
        .get("text_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let article_text = value
        .get("article_text")
        .and_then(serde_json::Value::as_str)
        .map(|text| clamp_chars(text.trim(), SCRAPE_ARTICLE_TEXT_MAX_CHARS));
    let article_text_chars = article_text
        .as_deref()
        .map(|text| text.chars().count())
        .unwrap_or(0);
    let article_text_truncated = value
        .get("article_text_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let article_status = match article_text.as_deref() {
        Some(text) if !text.is_empty() => "captured",
        Some(_) => "no_article",
        None => "not_returned",
    };
    let article_selector = clamp_chars(
        value
            .get("article_selector")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        80,
    );
    let canonical_url = clamp_chars(
        value
            .get("canonical_url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        MEDIA_SNIFFER_URL_MAX_CHARS,
    );
    let meta_description = clamp_chars(
        value
            .get("meta_description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        512,
    );
    let document_lang = clamp_chars(
        value
            .get("document_lang")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        64,
    );
    let mut links = Vec::new();
    if let Some(items) = value.get("links").and_then(serde_json::Value::as_array) {
        for item in items.iter().take(SCRAPE_DOM_LINK_MAX_COUNT) {
            let Some(raw_url) = item.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let url = clamp_chars(raw_url.trim(), MEDIA_SNIFFER_URL_MAX_CHARS);
            if url.is_empty() {
                continue;
            }
            links.push(ScrapeDomLink {
                same_origin: scrape_url_same_origin(page_url, &url),
                url,
                text: clamp_chars(
                    item.get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim(),
                    SCRAPE_DOM_TEXT_MAX_CHARS,
                ),
                rel: clamp_chars(
                    item.get("rel")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim(),
                    80,
                ),
                target: clamp_chars(
                    item.get("target")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim(),
                    40,
                ),
            });
        }
    }
    let mut headings = Vec::new();
    if let Some(items) = value.get("headings").and_then(serde_json::Value::as_array) {
        for item in items.iter().take(SCRAPE_DOM_HEADING_MAX_COUNT) {
            let text = clamp_chars(
                item.get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim(),
                SCRAPE_DOM_TEXT_MAX_CHARS,
            );
            if text.is_empty() {
                continue;
            }
            let level = item
                .get("level")
                .and_then(serde_json::Value::as_u64)
                .and_then(|level| u8::try_from(level).ok())
                .filter(|level| (1..=6).contains(level))
                .unwrap_or(0);
            headings.push(ScrapeDomHeading { level, text });
        }
    }
    let status = if links.is_empty() && headings.is_empty() {
        "no_dom"
    } else {
        "captured"
    };
    Ok(ScrapeDomExtract {
        status,
        text: Some(text),
        text_truncated,
        article_status,
        article_text,
        article_text_chars,
        article_text_truncated,
        article_selector,
        canonical_url,
        meta_description,
        document_lang,
        links,
        headings,
    })
}

fn scrape_url_same_origin(page_url: &str, candidate_url: &str) -> bool {
    let (Ok(page), Ok(candidate)) = (
        reqwest::Url::parse(page_url),
        reqwest::Url::parse(candidate_url),
    ) else {
        return false;
    };
    page.scheme().eq_ignore_ascii_case(candidate.scheme())
        && page.host_str().map(str::to_ascii_lowercase)
            == candidate.host_str().map(str::to_ascii_lowercase)
        && page.port_or_known_default() == candidate.port_or_known_default()
}

fn csv_cell(text: &str) -> String {
    let escaped = text.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn markdown_heading_text(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\r' | '\n' => ' ',
            _ => ch,
        })
        .collect::<String>()
}

fn markdown_inline_text(text: &str) -> String {
    text.replace('[', "\\[")
        .replace(']', "\\]")
        .replace('`', "\\`")
}

pub(super) fn active_page_media_manifest(
    url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Result<Vec<u8>, String> {
    let label = if title.trim().is_empty() {
        host_of(url).unwrap_or_else(|| "Untitled page".to_owned())
    } else {
        title.trim().to_owned()
    };
    let items = media_manifest_items(recent);
    serde_json::to_vec_pretty(&serde_json::json!({
        "op": "browser_media_manifest",
        "scope": "active_page_media_sniffer",
        "url": url,
        "title": label,
        "engine": engine.wire(),
        "captured_ms": unix_ms,
        "item_count": items.len(),
        "items": items,
    }))
    .map_err(|err| format!("encode media manifest JSON: {err}"))
}

fn media_manifest_items(
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Vec<serde_json::Value> {
    recent
        .iter()
        .rev()
        .filter_map(|resource| {
            let url = resource.url.trim();
            let kind = media_candidate_kind(resource.resource, url)?;
            Some(serde_json::json!({
                "url": clamp_chars(url, MEDIA_SNIFFER_URL_MAX_CHARS),
                "resource": offline_cache_resource_type_name(resource.resource),
                "kind": kind,
                "allowed": resource.allowed,
                "blocked_by": resource.blocked_by.as_deref(),
                "filename_hint": media_filename_hint(url),
            }))
        })
        .take(MEDIA_SNIFFER_MAX_COUNT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
pub(super) fn active_page_media_asset_requests(
    page_url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Result<Vec<Vec<u8>>, String> {
    active_page_media_asset_requests_with_selection(
        page_url,
        title,
        engine,
        unix_ms,
        recent,
        MediaAssetSelection::All,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaAssetSelection {
    All,
    Images,
}

impl MediaAssetSelection {
    fn accepts(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Images => matches!(kind, "image"),
        }
    }

    pub(super) const fn empty_error(self) -> &'static str {
        match self {
            Self::All => "no observed media/image assets to queue",
            Self::Images => "no observed image assets to queue",
        }
    }
}

pub(super) fn active_page_media_asset_requests_with_selection(
    page_url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
    selection: MediaAssetSelection,
) -> Result<Vec<Vec<u8>>, String> {
    let label = if title.trim().is_empty() {
        host_of(page_url).unwrap_or_else(|| "Untitled page".to_owned())
    } else {
        title.trim().to_owned()
    };
    let mut seen = BTreeSet::new();
    let mut requests = Vec::new();
    for resource in recent.iter().rev() {
        if requests.len() >= MEDIA_SNIFFER_MAX_COUNT {
            break;
        }
        let asset_url = resource.url.trim();
        if asset_url.is_empty() || !seen.insert(asset_url.to_owned()) {
            continue;
        }
        let Some(kind) = media_candidate_kind(resource.resource, asset_url) else {
            continue;
        };
        if !selection.accepts(kind) {
            continue;
        }
        let filename_hint = media_filename_hint(asset_url);
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "op": "browser_media_download_request",
            "scope": "observed_media_asset",
            "source": "browser_power_mode",
            "page_url": page_url,
            "page_title": label,
            "engine": engine.wire(),
            "captured_ms": unix_ms,
            "asset_url": clamp_chars(asset_url, MEDIA_SNIFFER_URL_MAX_CHARS),
            "resource": offline_cache_resource_type_name(resource.resource),
            "kind": kind,
            "allowed_by_page_filter": resource.allowed,
            "blocked_by_page_filter": resource.blocked_by.as_deref(),
            "ignore_blocking": !resource.allowed,
            "suggested_filename": filename_hint,
            "rename_strategy": "auto_rename_by_url_hint",
            "retrieval": "native_media_downloader_request",
        }))
        .map_err(|err| format!("encode media download request: {err}"))?;
        requests.push(body);
    }
    requests.reverse();
    Ok(requests)
}

fn media_candidate_kind(resource: u8, url: &str) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    if lower.contains(".m3u8") {
        return Some("hls");
    }
    if lower.contains(".mpd") {
        return Some("dash");
    }
    if media_url_has_any_suffix(&lower, &[".mp4", ".m4v", ".webm", ".mov", ".m4s", ".ts"]) {
        return Some("video");
    }
    if media_url_has_any_suffix(&lower, &[".mp3", ".m4a", ".aac", ".ogg", ".opus", ".flac"]) {
        return Some("audio");
    }
    if media_url_has_any_suffix(
        &lower,
        &[
            ".png", ".jpg", ".jpeg", ".webp", ".gif", ".avif", ".svg", ".bmp",
        ],
    ) {
        return Some("image");
    }
    match mde_web_preview_client::resource_from_wire(resource) {
        mde_web_preview_client::ResourceType::Media => Some("media"),
        mde_web_preview_client::ResourceType::Image => Some("image"),
        _ => None,
    }
}

fn sanitize_filename_component(text: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in text.chars() {
        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            last_dash = false;
            Some(ch)
        } else if !last_dash {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = next {
            out.push(ch);
        }
        if out.len() >= max_len {
            break;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "media".to_owned()
    } else {
        out.to_owned()
    }
}

fn media_url_has_any_suffix(lower_url: &str, suffixes: &[&str]) -> bool {
    let path = lower_url.split(['?', '#']).next().unwrap_or(lower_url);
    suffixes.iter().any(|suffix| path.ends_with(suffix))
}

fn media_filename_hint(url: &str) -> String {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/');
    let leaf = path.rsplit('/').next().unwrap_or("media");
    let decoded = leaf.replace("%20", " ");
    let mut out = String::new();
    let mut last_dash = false;
    for ch in decoded.chars() {
        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            last_dash = false;
            Some(ch)
        } else if !last_dash {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = next {
            out.push(ch);
        }
        if out.len() >= 96 {
            break;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "media".to_owned()
    } else {
        out.to_owned()
    }
}

pub(super) fn capture_annotation_caption(url: &str, title: &str, unix_ms: u64) -> String {
    let title = title.trim();
    let label = if title.is_empty() {
        host_of(url).unwrap_or_else(|| "page".to_owned())
    } else {
        title.to_owned()
    };
    format!("{label} | {url} | {unix_ms}")
}

pub(super) fn mhtml_capture_document(url: &str, title: &str, unix_ms: u64, png: &[u8]) -> Vec<u8> {
    const BOUNDARY: &str = "----=_MagicMeshBrowserCapture";
    const IMAGE_LOCATION: &str = "mde-browser-capture.png";
    let title = title.trim();
    let label = if title.is_empty() {
        host_of(url).unwrap_or_else(|| "Browser Capture".to_owned())
    } else {
        title.to_owned()
    };
    let html = format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<title>{title}</title></head><body>",
            "<h1>{title}</h1>",
            "<p>Captured from <a href=\"{url}\">{url}</a></p>",
            "<p>Capture time: {unix_ms}</p>",
            "<img src=\"{image_location}\" alt=\"Browser capture\">",
            "</body></html>"
        ),
        title = html_escape(&label),
        url = html_escape(url),
        unix_ms = unix_ms,
        image_location = IMAGE_LOCATION
    );
    let encoded_png = base64::engine::general_purpose::STANDARD.encode(png);
    let mut out = String::new();
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str(&format!(
        "Content-Type: multipart/related; type=\"text/html\"; boundary=\"{BOUNDARY}\"\r\n"
    ));
    out.push_str(&format!(
        "Subject: {} Capture - {}\r\n\r\n",
        browser_product_label(),
        mhtml_header_value(&html_escape(&label))
    ));
    out.push_str(&format!("--{BOUNDARY}\r\n"));
    out.push_str("Content-Type: text/html; charset=\"utf-8\"\r\n");
    out.push_str("Content-Transfer-Encoding: 8bit\r\n");
    out.push_str(&format!(
        "Content-Location: {}\r\n\r\n",
        if url.trim().is_empty() {
            "about:blank"
        } else {
            url.trim()
        }
    ));
    out.push_str(&html);
    out.push_str("\r\n");
    out.push_str(&format!("--{BOUNDARY}\r\n"));
    out.push_str("Content-Type: image/png\r\n");
    out.push_str("Content-Transfer-Encoding: base64\r\n");
    out.push_str(&format!("Content-Location: {IMAGE_LOCATION}\r\n\r\n"));
    for chunk in encoded_png.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\r\n");
    }
    out.push_str(&format!("--{BOUNDARY}--\r\n"));
    out.into_bytes()
}

pub(super) fn offline_cache_mhtml_document(
    url: &str,
    title: &str,
    unix_ms: u64,
    text: &str,
    viewport_png: Option<&[u8]>,
) -> Vec<u8> {
    const BOUNDARY: &str = "----=_MagicMeshBrowserOfflineCache";
    const IMAGE_LOCATION: &str = "mde-browser-offline-viewport.png";
    let title = title.trim();
    let label = if title.is_empty() {
        host_of(url).unwrap_or_else(|| "Browser Offline Copy".to_owned())
    } else {
        title.to_owned()
    };
    let image_markup = viewport_png
        .map(|_| "<img src=\"mde-browser-offline-viewport.png\" alt=\"Cached viewport\">")
        .unwrap_or("");
    let html = format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<title>{title}</title></head><body>",
            "<h1>{title}</h1>",
            "<p>Offline copy from <a href=\"{url}\">{url}</a></p>",
            "<p>Capture time: {unix_ms}</p>",
            "{image_markup}",
            "<pre>{text}</pre>",
            "</body></html>"
        ),
        title = html_escape(&label),
        url = html_escape(url),
        unix_ms = unix_ms,
        image_markup = image_markup,
        text = html_escape(text)
    );
    let mut out = String::new();
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str(&format!(
        "Content-Type: multipart/related; type=\"text/html\"; boundary=\"{BOUNDARY}\"\r\n"
    ));
    out.push_str(&format!(
        "Subject: {} Offline Copy - {}\r\n\r\n",
        browser_product_label(),
        mhtml_header_value(&html_escape(&label))
    ));
    out.push_str(&format!("--{BOUNDARY}\r\n"));
    out.push_str("Content-Type: text/html; charset=\"utf-8\"\r\n");
    out.push_str("Content-Transfer-Encoding: 8bit\r\n");
    out.push_str(&format!(
        "Content-Location: {}\r\n\r\n",
        if url.trim().is_empty() {
            "about:blank"
        } else {
            url.trim()
        }
    ));
    out.push_str(&html);
    out.push_str("\r\n");
    if let Some(png) = viewport_png {
        let encoded_png = base64::engine::general_purpose::STANDARD.encode(png);
        out.push_str(&format!("--{BOUNDARY}\r\n"));
        out.push_str("Content-Type: image/png\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n");
        out.push_str(&format!("Content-Location: {IMAGE_LOCATION}\r\n\r\n"));
        for chunk in encoded_png.as_bytes().chunks(76) {
            out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            out.push_str("\r\n");
        }
    }
    out.push_str(&format!("--{BOUNDARY}--\r\n"));
    out.into_bytes()
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn mhtml_header_value(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect()
}

pub(super) fn process_error(command: &str, output: &ProcessOutput) -> String {
    let err = output.stderr.trim();
    if err.is_empty() {
        format!("{command} failed without an error message")
    } else {
        err.to_owned()
    }
}

pub(super) fn run_process_with_timeout(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<ProcessOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{program} failed to start: {err}"))?;
    let started = Instant::now();
    while started.elapsed() < timeout {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|err| format!("{program} output failed: {err}"))?;
                return Ok(ProcessOutput {
                    success: output.status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => return Err(format!("{program} status failed: {err}")),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(format!("{program} timed out after {}s", timeout.as_secs()))
}

pub(super) fn encode_color_image_png(img: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let [w, h] = img.size;
    if w == 0 || h == 0 {
        return Err("empty frame".to_owned());
    }
    let expected = w
        .checked_mul(h)
        .ok_or_else(|| "frame dimensions overflow".to_owned())?;
    if img.pixels.len() != expected {
        return Err(format!(
            "frame has {} pixels but expected {expected}",
            img.pixels.len()
        ));
    }
    let mut rgba = Vec::with_capacity(expected * 4);
    for pixel in &img.pixels {
        rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
    }
    let mut bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut bytes, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|err| format!("could not write PNG header: {err}"))?;
        writer
            .write_image_data(&rgba)
            .map_err(|err| format!("could not write PNG pixels: {err}"))?;
    }
    Ok(bytes)
}

pub(super) const ANNOTATION_BAR_HEIGHT: usize = 24;
const ANNOTATION_TEXT_SCALE: usize = 2;

pub(super) fn annotate_capture_image(
    img: &egui::ColorImage,
    caption: &str,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    if w == 0 || h == 0 {
        return Err("empty frame".to_owned());
    }
    let out_h = h
        .checked_add(ANNOTATION_BAR_HEIGHT)
        .ok_or_else(|| "annotated frame dimensions overflow".to_owned())?;
    let expected = w
        .checked_mul(h)
        .ok_or_else(|| "frame dimensions overflow".to_owned())?;
    if img.pixels.len() != expected {
        return Err(format!(
            "frame has {} pixels but expected {expected}",
            img.pixels.len()
        ));
    }
    let mut out = egui::ColorImage::new([w, out_h], Style::BG);
    out.pixels[..expected].copy_from_slice(&img.pixels);
    for y in h..out_h {
        for x in 0..w {
            out.pixels[y * w + x] = if y == h {
                Style::ACCENT
            } else {
                Style::SURFACE
            };
        }
    }
    draw_tiny_text(
        &mut out,
        6,
        h + 6,
        &caption.to_ascii_uppercase(),
        Style::TEXT,
    );
    Ok(out)
}

pub(super) fn annotate_callout_capture_image(
    img: &egui::ColorImage,
    caption: &str,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    let mut out = annotate_capture_image(img, caption)?;
    if w < 16 || h < 12 {
        draw_tiny_text(&mut out, 6, h + 6, "CALLOUT", Style::TEXT_STRONG);
        return Ok(out);
    }

    let box_w = (w / 3).clamp(12, 180);
    let box_h = (h / 3).clamp(8, 96);
    let x = (w.saturating_sub(box_w)) / 2;
    let y = (h.saturating_sub(box_h)) / 2;
    let accent = Style::ACCENT;
    draw_rect_outline(&mut out, x, y, box_w, box_h, accent);

    let leader_start_x = x.saturating_add(box_w);
    let leader_start_y = y;
    let leader_end_x = w.saturating_sub(3);
    let leader_end_y = 3;
    draw_diagonal_line(
        &mut out,
        leader_start_x,
        leader_start_y,
        leader_end_x,
        leader_end_y,
        accent,
    );
    draw_rect_outline(
        &mut out,
        leader_end_x.saturating_sub(10),
        leader_end_y,
        10,
        8,
        accent,
    );
    draw_tiny_text(
        &mut out,
        leader_end_x.saturating_sub(8),
        leader_end_y.saturating_add(1),
        "1",
        Style::TEXT_STRONG,
    );
    draw_tiny_text(&mut out, 6, h + 6, "CALLOUT", Style::TEXT_STRONG);
    Ok(out)
}

pub(super) fn annotate_freehand_capture_image(
    img: &egui::ColorImage,
    caption: &str,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    let mut out = annotate_capture_image(img, caption)?;
    let stroke = Style::TEXT_STRONG;
    if w < 16 || h < 12 {
        draw_tiny_text(&mut out, 6, h + 6, "FREEHAND", stroke);
        return Ok(out);
    }

    let left = w / 5;
    let right = w.saturating_sub(left.max(1));
    let top = h / 4;
    let mid = h / 2;
    let bottom = h.saturating_sub(top.max(1));
    let points = [
        (left, mid),
        (left.saturating_add(w / 10), top),
        (left.saturating_add(w / 4), bottom),
        (left.saturating_add(w / 2), top.saturating_add(h / 8)),
        (right, mid),
    ];
    for segment in points.windows(2) {
        let [(x0, y0), (x1, y1)] = segment else {
            continue;
        };
        draw_thick_line(&mut out, *x0, *y0, *x1, *y1, stroke);
    }
    draw_tiny_text(&mut out, 6, h + 6, "FREEHAND", stroke);
    Ok(out)
}

fn draw_thick_line(
    img: &mut egui::ColorImage,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    color: egui::Color32,
) {
    for (dx, dy) in [(0isize, 0isize), (1, 0), (-1, 0), (0, 1), (0, -1)] {
        let sx0 = offset_coord(x0, dx);
        let sy0 = offset_coord(y0, dy);
        let sx1 = offset_coord(x1, dx);
        let sy1 = offset_coord(y1, dy);
        draw_diagonal_line(img, sx0, sy0, sx1, sy1, color);
    }
}

fn offset_coord(value: usize, delta: isize) -> usize {
    if delta.is_negative() {
        value.saturating_sub(delta.unsigned_abs())
    } else {
        value.saturating_add(delta as usize)
    }
}

fn draw_rect_outline(
    img: &mut egui::ColorImage,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: egui::Color32,
) {
    if width == 0 || height == 0 {
        return;
    }
    let right = x.saturating_add(width.saturating_sub(1));
    let bottom = y.saturating_add(height.saturating_sub(1));
    for px in x..=right {
        set_pixel(img, px, y, color);
        set_pixel(img, px, bottom, color);
    }
    for py in y..=bottom {
        set_pixel(img, x, py, color);
        set_pixel(img, right, py, color);
    }
}

fn draw_diagonal_line(
    img: &mut egui::ColorImage,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    color: egui::Color32,
) {
    let mut x0 = isize::try_from(x0).unwrap_or(isize::MAX);
    let mut y0 = isize::try_from(y0).unwrap_or(isize::MAX);
    let x1 = isize::try_from(x1).unwrap_or(isize::MAX);
    let y1 = isize::try_from(y1).unwrap_or(isize::MAX);
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if let (Ok(x), Ok(y)) = (usize::try_from(x0), usize::try_from(y0)) {
            set_pixel(img, x, y, color);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = err.saturating_mul(2);
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn set_pixel(img: &mut egui::ColorImage, x: usize, y: usize, color: egui::Color32) {
    let [w, h] = img.size;
    if x < w && y < h {
        img.pixels[y * w + x] = color;
    }
}

fn draw_tiny_text(
    img: &mut egui::ColorImage,
    mut x: usize,
    y: usize,
    text: &str,
    color: egui::Color32,
) {
    for ch in text.chars() {
        let glyph = tiny_glyph(ch);
        draw_tiny_glyph(img, x, y, glyph, color);
        x = x.saturating_add(6 * ANNOTATION_TEXT_SCALE);
        if x + 5 * ANNOTATION_TEXT_SCALE >= img.size[0] {
            break;
        }
    }
}

fn draw_tiny_glyph(
    img: &mut egui::ColorImage,
    x: usize,
    y: usize,
    glyph: [&'static str; 7],
    color: egui::Color32,
) {
    let [w, h] = img.size;
    for (gy, row) in glyph.iter().enumerate() {
        for (gx, bit) in row.as_bytes().iter().enumerate() {
            if *bit != b'1' {
                continue;
            }
            for sy in 0..ANNOTATION_TEXT_SCALE {
                for sx in 0..ANNOTATION_TEXT_SCALE {
                    let px = x + gx * ANNOTATION_TEXT_SCALE + sx;
                    let py = y + gy * ANNOTATION_TEXT_SCALE + sy;
                    if px < w && py < h {
                        img.pixels[py * w + px] = color;
                    }
                }
            }
        }
    }
}

fn tiny_glyph(ch: char) -> [&'static str; 7] {
    match ch {
        'A' => [
            "01110", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'B' => [
            "11110", "10001", "10001", "11110", "10001", "10001", "11110",
        ],
        'C' => [
            "01111", "10000", "10000", "10000", "10000", "10000", "01111",
        ],
        'D' => [
            "11110", "10001", "10001", "10001", "10001", "10001", "11110",
        ],
        'E' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
        'F' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "10000",
        ],
        'G' => [
            "01111", "10000", "10000", "10111", "10001", "10001", "01111",
        ],
        'H' => [
            "10001", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'I' => [
            "11111", "00100", "00100", "00100", "00100", "00100", "11111",
        ],
        'J' => [
            "00111", "00010", "00010", "00010", "00010", "10010", "01100",
        ],
        'K' => [
            "10001", "10010", "10100", "11000", "10100", "10010", "10001",
        ],
        'L' => [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
        'M' => [
            "10001", "11011", "10101", "10101", "10001", "10001", "10001",
        ],
        'N' => [
            "10001", "11001", "10101", "10011", "10001", "10001", "10001",
        ],
        'O' => [
            "01110", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'P' => [
            "11110", "10001", "10001", "11110", "10000", "10000", "10000",
        ],
        'Q' => [
            "01110", "10001", "10001", "10001", "10101", "10010", "01101",
        ],
        'R' => [
            "11110", "10001", "10001", "11110", "10100", "10010", "10001",
        ],
        'S' => [
            "01111", "10000", "10000", "01110", "00001", "00001", "11110",
        ],
        'T' => [
            "11111", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
        'U' => [
            "10001", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'V' => [
            "10001", "10001", "10001", "10001", "10001", "01010", "00100",
        ],
        'W' => [
            "10001", "10001", "10001", "10101", "10101", "10101", "01010",
        ],
        'X' => [
            "10001", "10001", "01010", "00100", "01010", "10001", "10001",
        ],
        'Y' => [
            "10001", "10001", "01010", "00100", "00100", "00100", "00100",
        ],
        'Z' => [
            "11111", "00001", "00010", "00100", "01000", "10000", "11111",
        ],
        '0' => [
            "01110", "10001", "10011", "10101", "11001", "10001", "01110",
        ],
        '1' => [
            "00100", "01100", "00100", "00100", "00100", "00100", "01110",
        ],
        '2' => [
            "01110", "10001", "00001", "00010", "00100", "01000", "11111",
        ],
        '3' => [
            "11110", "00001", "00001", "01110", "00001", "00001", "11110",
        ],
        '4' => [
            "00010", "00110", "01010", "10010", "11111", "00010", "00010",
        ],
        '5' => [
            "11111", "10000", "10000", "11110", "00001", "00001", "11110",
        ],
        '6' => [
            "01110", "10000", "10000", "11110", "10001", "10001", "01110",
        ],
        '7' => [
            "11111", "00001", "00010", "00100", "01000", "01000", "01000",
        ],
        '8' => [
            "01110", "10001", "10001", "01110", "10001", "10001", "01110",
        ],
        '9' => [
            "01110", "10001", "10001", "01111", "00001", "00001", "01110",
        ],
        ':' => [
            "00000", "00100", "00100", "00000", "00100", "00100", "00000",
        ],
        '/' => [
            "00001", "00010", "00010", "00100", "01000", "01000", "10000",
        ],
        '.' => [
            "00000", "00000", "00000", "00000", "00000", "01100", "01100",
        ],
        '-' => [
            "00000", "00000", "00000", "11111", "00000", "00000", "00000",
        ],
        '_' => [
            "00000", "00000", "00000", "00000", "00000", "00000", "11111",
        ],
        '|' => [
            "00100", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
        ' ' => [
            "00000", "00000", "00000", "00000", "00000", "00000", "00000",
        ],
        _ => [
            "00000", "00000", "00000", "01110", "00000", "00000", "00000",
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PixelRegion {
    pub(super) x: usize,
    pub(super) y: usize,
    pub(super) width: usize,
    pub(super) height: usize,
}

impl PixelRegion {
    pub(super) fn from_points(
        a: egui::Pos2,
        b: egui::Pos2,
        frame_size: [usize; 2],
    ) -> Option<Self> {
        let [frame_w, frame_h] = frame_size;
        if frame_w == 0 || frame_h == 0 {
            return None;
        }
        let min_x = a.x.min(b.x).floor().clamp(0.0, frame_w as f32) as usize;
        let min_y = a.y.min(b.y).floor().clamp(0.0, frame_h as f32) as usize;
        let max_x = a.x.max(b.x).ceil().clamp(0.0, frame_w as f32) as usize;
        let max_y = a.y.max(b.y).ceil().clamp(0.0, frame_h as f32) as usize;
        let width = max_x.saturating_sub(min_x);
        let height = max_y.saturating_sub(min_y);
        (width > 1 && height > 1).then_some(Self {
            x: min_x,
            y: min_y,
            width,
            height,
        })
    }

    pub(super) fn rect_on_image(
        self,
        image_rect: egui::Rect,
        frame_size: [usize; 2],
    ) -> egui::Rect {
        let [frame_w, frame_h] = frame_size;
        let sx = image_rect.width() / frame_w.max(1) as f32;
        let sy = image_rect.height() / frame_h.max(1) as f32;
        egui::Rect::from_min_size(
            image_rect.min + egui::vec2(self.x as f32 * sx, self.y as f32 * sy),
            egui::vec2(self.width as f32 * sx, self.height as f32 * sy),
        )
    }
}

pub(super) fn crop_color_image(
    img: &egui::ColorImage,
    region: PixelRegion,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    if region.x >= w
        || region.y >= h
        || region.width == 0
        || region.height == 0
        || region.x + region.width > w
        || region.y + region.height > h
    {
        return Err("capture region is outside the active frame".to_owned());
    }
    let mut pixels = Vec::with_capacity(region.width * region.height);
    for row in region.y..region.y + region.height {
        let start = row * w + region.x;
        let end = start + region.width;
        pixels.extend_from_slice(&img.pixels[start..end]);
    }
    let mut out = egui::ColorImage::new([region.width, region.height], egui::Color32::TRANSPARENT);
    out.pixels = pixels;
    Ok(out)
}

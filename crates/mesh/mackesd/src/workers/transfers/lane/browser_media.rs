//! TRANSFERS browser-media manifest cluster — the leaf HLS/DASH/scrape-crawl
//! request parsing plus playlist/MPD/XML rewriting split out of the `lane` worker
//! god-module (pure relocation, no behaviour change).
//!
//! `use super::*` pulls in the parent `lane` module's std/lane imports; as a child
//! module it reads the parent's private items directly, so only the request/manifest
//! types and entry helpers the parent (and the tests) call back into are `pub(super)`.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BrowserMediaDownloadRequest {
    pub(super) asset_url: String,
    pub(super) suggested_filename: String,
    pub(super) kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BrowserScrapeCrawlRequest {
    pub(super) page_url: String,
    pub(super) targets: Vec<BrowserScrapeCrawlTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BrowserScrapeCrawlTarget {
    pub(super) url: String,
    pub(super) source: String,
    pub(super) resource: String,
    pub(super) depth: u8,
}

pub(super) fn browser_media_download_request(
    source: &Path,
) -> Result<Option<BrowserMediaDownloadRequest>, String> {
    if source.extension().and_then(|ext| ext.to_str()) != Some("json")
        || !source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".download.json"))
    {
        return Ok(None);
    }
    let body = std::fs::read(source).map_err(|e| {
        format!(
            "browser media request {} could not be read: {e}",
            source.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        format!(
            "browser media request {} is not JSON: {e}",
            source.display()
        )
    })?;
    if value.get("op").and_then(serde_json::Value::as_str) != Some("browser_media_download_request")
    {
        return Ok(None);
    }
    let asset_url = value
        .get("asset_url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| is_http_url(url))
        .ok_or_else(|| {
            "browser media request requires an http:// or https:// asset_url".to_string()
        })?
        .to_owned();
    if asset_url.as_bytes().contains(&0) {
        return Err("browser media request rejects NUL bytes in asset_url".to_string());
    }
    let suggested = value
        .get("suggested_filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("browser-media");
    let suggested_filename = safe_browser_download_filename(suggested);
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(|kind| kind.to_ascii_lowercase());
    Ok(Some(BrowserMediaDownloadRequest {
        asset_url,
        suggested_filename,
        kind,
    }))
}

pub(super) fn browser_scrape_crawl_request(
    source: &Path,
) -> Result<Option<BrowserScrapeCrawlRequest>, String> {
    if source.extension().and_then(|ext| ext.to_str()) != Some("json") {
        return Ok(None);
    }
    if !source
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("mde-browser-scrape-"))
    {
        return Ok(None);
    }
    let body = std::fs::read(source).map_err(|e| {
        format!(
            "browser scrape crawl request {} could not be read: {e}",
            source.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        format!(
            "browser scrape crawl request {} is not JSON: {e}",
            source.display()
        )
    })?;
    if value.get("op").and_then(serde_json::Value::as_str) != Some("browser_active_page_scrape") {
        return Ok(None);
    }
    if value
        .get("crawl_execution_status")
        .and_then(serde_json::Value::as_str)
        != Some("not_started")
    {
        return Ok(None);
    }
    let page_url = value
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| is_http_url(url))
        .ok_or_else(|| {
            "browser scrape crawl request requires an http:// or https:// page url".to_string()
        })?
        .to_owned();
    if page_url.as_bytes().contains(&0) {
        return Err("browser scrape crawl request rejects NUL bytes in page url".to_string());
    }
    let page = reqwest::Url::parse(&page_url)
        .map_err(|e| format!("browser scrape crawl page URL is invalid: {e}"))?;
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    if let Some(items) = value
        .get("crawl_manifest")
        .and_then(serde_json::Value::as_array)
    {
        for item in items.iter() {
            if targets.len() >= BROWSER_SCRAPE_CRAWL_MAX_TARGETS {
                break;
            }
            if item.get("same_origin").and_then(serde_json::Value::as_bool) != Some(true) {
                continue;
            }
            let Some(raw_url) = item.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let url = raw_url.trim();
            if !is_http_url(url) || url.as_bytes().contains(&0) {
                continue;
            }
            if !same_origin_url(&page, url) {
                continue;
            }
            if !seen.insert(url.to_owned()) {
                continue;
            }
            targets.push(BrowserScrapeCrawlTarget {
                url: url.to_owned(),
                source: item
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|source| !source.is_empty())
                    .unwrap_or("crawl_manifest")
                    .chars()
                    .take(80)
                    .collect(),
                resource: item
                    .get("resource")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|resource| !resource.is_empty())
                    .unwrap_or("document")
                    .chars()
                    .take(80)
                    .collect(),
                depth: 1,
            });
        }
    }
    Ok(Some(BrowserScrapeCrawlRequest { page_url, targets }))
}

pub(super) fn browser_media_request_is_hls(request: &BrowserMediaDownloadRequest) -> bool {
    request.kind.as_deref() == Some("hls")
        || is_hls_manifest_url(&request.asset_url)
        || request
            .suggested_filename
            .to_ascii_lowercase()
            .ends_with(".m3u8")
}

pub(super) fn browser_media_request_is_dash(request: &BrowserMediaDownloadRequest) -> bool {
    request.kind.as_deref() == Some("dash")
        || is_dash_manifest_url(&request.asset_url)
        || request
            .suggested_filename
            .to_ascii_lowercase()
            .ends_with(".mpd")
}

fn same_origin_url(page: &reqwest::Url, candidate: &str) -> bool {
    let Ok(candidate) = reqwest::Url::parse(candidate) else {
        return false;
    };
    page.scheme().eq_ignore_ascii_case(candidate.scheme())
        && page.host_str().map(str::to_ascii_lowercase)
            == candidate.host_str().map(str::to_ascii_lowercase)
        && page.port_or_known_default() == candidate.port_or_known_default()
}

pub(super) fn is_hls_manifest_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(Iterator::last)
                .map(str::to_ascii_lowercase)
        })
        .is_some_and(|leaf| leaf.ends_with(".m3u8"))
}

fn is_dash_manifest_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(Iterator::last)
                .map(str::to_ascii_lowercase)
        })
        .is_some_and(|leaf| leaf.ends_with(".mpd"))
}

pub(super) fn hls_package_destination(dest: &Path, suggested_filename: &str) -> (PathBuf, String) {
    let manifest_filename = safe_browser_download_filename(suggested_filename);
    let stem = Path::new(&manifest_filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(safe_browser_download_filename)
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "browser-media".to_string());
    if dest.is_dir() {
        return (dest.join(format!("{stem}.hls")), manifest_filename);
    }
    let package_dir = dest.with_extension("hls");
    let filename = dest
        .file_name()
        .and_then(|name| name.to_str())
        .map(safe_browser_download_filename)
        .filter(|name| !name.is_empty())
        .unwrap_or(manifest_filename);
    (package_dir, filename)
}

pub(super) fn dash_package_destination(dest: &Path, suggested_filename: &str) -> (PathBuf, String) {
    let manifest_filename = safe_browser_download_filename(suggested_filename);
    let stem = Path::new(&manifest_filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(safe_browser_download_filename)
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "browser-media".to_string());
    if dest.is_dir() {
        return (dest.join(format!("{stem}.dash")), manifest_filename);
    }
    let package_dir = dest.with_extension("dash");
    let filename = dest
        .file_name()
        .and_then(|name| name.to_str())
        .map(safe_browser_download_filename)
        .filter(|name| !name.is_empty())
        .unwrap_or(manifest_filename);
    (package_dir, filename)
}

pub(super) fn scrape_crawl_package_destination(original_dest: &Path) -> PathBuf {
    if original_dest.is_dir() {
        return original_dest.join("browser-scrape.crawl");
    }
    let stem = original_dest
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(safe_browser_download_filename)
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "browser-scrape".to_string());
    match original_dest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent.join(format!("{stem}.crawl")),
        None => PathBuf::from(format!("{stem}.crawl")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HlsReference {
    pub(super) uri: String,
    pub(super) attr: bool,
}

impl HlsReference {
    pub(super) fn item_kind(&self) -> &'static str {
        if self.attr {
            "hls-asset"
        } else {
            "segment"
        }
    }
}

pub(super) fn hls_playlist_references(body: &str) -> Vec<HlsReference> {
    let mut refs = Vec::new();
    for line in body.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            refs.extend(
                hls_uri_attributes(line)
                    .into_iter()
                    .map(|uri| HlsReference { uri, attr: true }),
            );
        } else {
            refs.push(HlsReference {
                uri: line.to_string(),
                attr: false,
            });
        }
    }
    refs
}

fn hls_uri_attributes(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(idx) = rest.find("URI=") {
        rest = &rest[idx + 4..];
        let (uri, next) = if let Some(quoted) = rest.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((uri, tail)) => (uri, tail),
                None => break,
            }
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        };
        let uri = uri.trim();
        if !uri.is_empty() {
            out.push(uri.to_string());
        }
        rest = next;
    }
    out
}

pub(super) fn rewrite_hls_playlist_to_local(
    base_url: &str,
    body: &str,
    url_paths: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = String::new();
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            out.push_str(raw_line);
        } else if line.starts_with('#') {
            out.push_str(&rewrite_hls_uri_attributes(base_url, raw_line, url_paths)?);
        } else {
            let resolved = resolve_hls_child_url(base_url, line)?;
            if let Some(path) = url_paths.get(&resolved) {
                out.push_str(path);
            } else {
                out.push_str(raw_line);
            }
        }
        out.push('\n');
    }
    Ok(out)
}

fn rewrite_hls_uri_attributes(
    base_url: &str,
    line: &str,
    url_paths: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = line;
    while let Some(idx) = rest.find("URI=") {
        out.push_str(&rest[..idx + 4]);
        rest = &rest[idx + 4..];
        if let Some(quoted) = rest.strip_prefix('"') {
            let Some(end) = quoted.find('"') else {
                out.push('"');
                out.push_str(quoted);
                return Ok(out);
            };
            let raw_uri = &quoted[..end];
            out.push('"');
            out.push_str(&rewrite_hls_uri_value(base_url, raw_uri, url_paths)?);
            out.push('"');
            rest = &quoted[end + 1..];
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let raw_uri = &rest[..end];
            out.push_str(&rewrite_hls_uri_value(base_url, raw_uri, url_paths)?);
            rest = &rest[end..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn rewrite_hls_uri_value(
    base_url: &str,
    raw_uri: &str,
    url_paths: &BTreeMap<String, String>,
) -> Result<String, String> {
    let trimmed = raw_uri.trim();
    if trimmed.is_empty() {
        return Ok(raw_uri.to_string());
    }
    let resolved = resolve_hls_child_url(base_url, trimmed)?;
    Ok(url_paths
        .get(&resolved)
        .cloned()
        .unwrap_or_else(|| raw_uri.to_string()))
}

pub(super) fn resolve_hls_child_url(base_url: &str, child: &str) -> Result<String, String> {
    let base = reqwest::Url::parse(base_url)
        .map_err(|e| format!("browser HLS package base URL is invalid: {e}"))?;
    let child = child.trim();
    if child.as_bytes().contains(&0) {
        return Err("browser HLS package rejects NUL bytes in child URI".to_string());
    }
    let resolved = base
        .join(child)
        .map_err(|e| format!("browser HLS package child URI `{child}` is invalid: {e}"))?;
    if !matches!(resolved.scheme(), "http" | "https") {
        return Err("browser HLS package only follows http:// or https:// child URIs".to_string());
    }
    Ok(resolved.to_string())
}

pub(super) fn hls_url_filename(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(safe_browser_download_filename)
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "browser-media".to_string())
}

pub(super) fn unique_browser_package_filename(raw: &str, used: &mut BTreeSet<String>) -> String {
    let filename = safe_browser_download_filename(raw);
    if used.insert(filename.clone()) {
        return filename;
    }
    let path = Path::new(&filename);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("browser-media");
    let ext = path.extension().and_then(|ext| ext.to_str());
    for idx in 2..=BROWSER_HLS_MAX_ASSETS + BROWSER_HLS_MAX_PLAYLISTS + 2 {
        let candidate = match ext {
            Some(ext) if !ext.is_empty() => format!("{stem}-{idx}.{ext}"),
            _ => format!("{stem}-{idx}"),
        };
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    format!("browser-media-{}", used.len() + 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DashReference {
    pub(super) kind: &'static str,
    pub(super) url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DashRepresentation {
    id: String,
    bandwidth: String,
}

pub(super) fn dash_mpd_references(
    manifest_url: &str,
    body: &str,
) -> Result<Vec<DashReference>, String> {
    let manifest_base = reqwest::Url::parse(manifest_url)
        .map_err(|e| format!("browser DASH package MPD URL is invalid: {e}"))?;
    let base_urls = dash_base_urls(body);
    let bases = if base_urls.is_empty() {
        vec![manifest_base]
    } else {
        base_urls
            .iter()
            .map(|base| resolve_dash_child_url(manifest_base.as_str(), base))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|url| reqwest::Url::parse(&url).ok())
            .collect::<Vec<_>>()
    };
    let bases = if bases.is_empty() {
        vec![reqwest::Url::parse(manifest_url)
            .map_err(|e| format!("browser DASH package MPD URL is invalid: {e}"))?]
    } else {
        bases
    };
    let representations = dash_representations(body);
    let numbers = dash_segment_numbers(body);
    let mut out = Vec::new();

    for source in dash_source_urls(body) {
        for base in &bases {
            out.push(DashReference {
                kind: "dash-asset",
                url: resolve_dash_child_url(base.as_str(), &source)?,
            });
        }
    }

    for tag in xml_tags_named(body, "SegmentTemplate") {
        let start_number = xml_attr(&tag, "startNumber")
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(1);
        let numbers = if numbers.is_empty() {
            vec![start_number]
        } else {
            numbers.clone()
        };
        let init = xml_attr(&tag, "initialization");
        let media = xml_attr(&tag, "media");
        for representation in &representations {
            for base in &bases {
                if let Some(init) = init.as_deref() {
                    let uri = dash_expand_template(init, representation, start_number);
                    out.push(DashReference {
                        kind: "dash-init",
                        url: resolve_dash_child_url(base.as_str(), &uri)?,
                    });
                }
                if let Some(media) = media.as_deref() {
                    for number in &numbers {
                        let uri = dash_expand_template(media, representation, *number);
                        out.push(DashReference {
                            kind: "dash-segment",
                            url: resolve_dash_child_url(base.as_str(), &uri)?,
                        });
                    }
                }
            }
        }
    }

    Ok(out)
}

fn dash_base_urls(body: &str) -> Vec<String> {
    xml_element_texts(body, "BaseURL")
        .into_iter()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect()
}

fn dash_source_urls(body: &str) -> Vec<String> {
    xml_tags(body)
        .into_iter()
        .filter_map(|tag| xml_attr(&tag, "sourceURL"))
        .filter(|url| !url.trim().is_empty())
        .collect()
}

fn dash_representations(body: &str) -> Vec<DashRepresentation> {
    let reps = xml_tags_named(body, "Representation")
        .into_iter()
        .map(|tag| DashRepresentation {
            id: xml_attr(&tag, "id").unwrap_or_else(|| "representation".to_string()),
            bandwidth: xml_attr(&tag, "bandwidth").unwrap_or_else(|| "0".to_string()),
        })
        .collect::<Vec<_>>();
    if reps.is_empty() {
        vec![DashRepresentation {
            id: "representation".to_string(),
            bandwidth: "0".to_string(),
        }]
    } else {
        reps
    }
}

fn dash_segment_numbers(body: &str) -> Vec<u64> {
    let mut numbers = Vec::new();
    let mut next = xml_tags_named(body, "SegmentTemplate")
        .into_iter()
        .find_map(|tag| xml_attr(&tag, "startNumber"))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(1);
    for tag in xml_tags_named(body, "S") {
        let repeat = xml_attr(&tag, "r")
            .and_then(|r| r.parse::<i64>().ok())
            .map_or(1usize, |r| {
                if r < 0 {
                    1
                } else {
                    (r as usize).saturating_add(1)
                }
            });
        for _ in 0..repeat {
            if numbers.len() >= BROWSER_DASH_MAX_ASSETS {
                return numbers;
            }
            numbers.push(next);
            next = next.saturating_add(1);
        }
    }
    numbers
}

fn dash_expand_template(
    template: &str,
    representation: &DashRepresentation,
    number: u64,
) -> String {
    let with_rep = template
        .replace("$RepresentationID$", &representation.id)
        .replace("$Bandwidth$", &representation.bandwidth);
    dash_expand_number_token(&with_rep, number)
}

fn dash_expand_number_token(template: &str, number: u64) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("$Number") {
        out.push_str(&rest[..start]);
        let token_rest = &rest[start + 1..];
        let Some(end) = token_rest.find('$') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = &token_rest[..end];
        if let Some(width) = token
            .strip_prefix("Number%0")
            .and_then(|raw| raw.strip_suffix('d'))
            .and_then(|raw| raw.parse::<usize>().ok())
        {
            out.push_str(&format!("{number:0width$}"));
        } else {
            out.push_str(&number.to_string());
        }
        rest = &token_rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn resolve_dash_child_url(base_url: &str, child: &str) -> Result<String, String> {
    let base = reqwest::Url::parse(base_url)
        .map_err(|e| format!("browser DASH package base URL is invalid: {e}"))?;
    let child = xml_unescape(child.trim());
    if child.as_bytes().contains(&0) {
        return Err("browser DASH package rejects NUL bytes in child URI".to_string());
    }
    let resolved = base
        .join(&child)
        .map_err(|e| format!("browser DASH package child URI `{child}` is invalid: {e}"))?;
    if !matches!(resolved.scheme(), "http" | "https") {
        return Err("browser DASH package only follows http:// or https:// child URIs".to_string());
    }
    Ok(resolved.to_string())
}

pub(super) fn rewrite_dash_mpd_to_local(
    manifest_url: &str,
    body: &str,
    url_paths: &BTreeMap<String, String>,
) -> Result<String, String> {
    let base_urls = dash_base_urls(body);
    let manifest_base = reqwest::Url::parse(manifest_url)
        .map_err(|e| format!("browser DASH package MPD URL is invalid: {e}"))?;
    let mut bases = if base_urls.is_empty() {
        vec![manifest_base]
    } else {
        base_urls
            .iter()
            .map(|base| resolve_dash_child_url(manifest_base.as_str(), base))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|url| reqwest::Url::parse(&url).ok())
            .collect::<Vec<_>>()
    };
    if bases.is_empty() {
        bases.push(
            reqwest::Url::parse(manifest_url)
                .map_err(|e| format!("browser DASH package MPD URL is invalid: {e}"))?,
        );
    }
    let representations = dash_representations(body);
    let numbers = dash_segment_numbers(body);
    let mut replacements = BTreeMap::new();

    for source in dash_source_urls(body) {
        for base in &bases {
            let resolved = resolve_dash_child_url(base.as_str(), &source)?;
            if let Some(path) = url_paths.get(&resolved) {
                replacements.insert(source.clone(), path.clone());
            }
        }
    }

    for tag in xml_tags_named(body, "SegmentTemplate") {
        let start_number = xml_attr(&tag, "startNumber")
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(1);
        let numbers = if numbers.is_empty() {
            vec![start_number]
        } else {
            numbers.clone()
        };
        if let Some(init) = xml_attr(&tag, "initialization") {
            let rewrite_template = dash_local_asset_template(
                &init,
                &representations,
                start_number,
                &bases,
                url_paths,
            )?;
            if let Some(template) = rewrite_template {
                replacements.insert(init, template);
            }
        }
        if let Some(media) = xml_attr(&tag, "media") {
            let rewrite_template =
                dash_local_segment_template(&media, &representations, &numbers, &bases, url_paths)?;
            if let Some(template) = rewrite_template {
                replacements.insert(media, template);
            }
        }
    }

    let mut rewritten = rewrite_xml_element_texts(body, "BaseURL", "");
    for (from, to) in replacements {
        rewritten = rewritten.replace(&from, &xml_escape_attr(&to));
    }
    Ok(rewritten)
}

fn dash_local_asset_template(
    template: &str,
    representations: &[DashRepresentation],
    number: u64,
    bases: &[reqwest::Url],
    url_paths: &BTreeMap<String, String>,
) -> Result<Option<String>, String> {
    let Some(first_representation) = representations.first() else {
        return Ok(None);
    };
    let first_uri = dash_expand_template(template, first_representation, number);
    let mut first_path = None;
    for base in bases {
        let resolved = resolve_dash_child_url(base.as_str(), &first_uri)?;
        if let Some(path) = url_paths.get(&resolved) {
            first_path = Some(path.clone());
            break;
        }
    }
    let Some(first_path) = first_path else {
        return Ok(None);
    };
    Ok(Some(dash_localize_template_tokens(
        template,
        first_path,
        first_representation,
        number,
    )))
}

fn dash_local_segment_template(
    template: &str,
    representations: &[DashRepresentation],
    numbers: &[u64],
    bases: &[reqwest::Url],
    url_paths: &BTreeMap<String, String>,
) -> Result<Option<String>, String> {
    let Some(first_representation) = representations.first() else {
        return Ok(None);
    };
    let first_number = numbers.first().copied().unwrap_or(1);
    let first_uri = dash_expand_template(template, first_representation, first_number);
    let mut first_path = None;
    for base in bases {
        let resolved = resolve_dash_child_url(base.as_str(), &first_uri)?;
        if let Some(path) = url_paths.get(&resolved) {
            first_path = Some(path.clone());
            break;
        }
    }
    let Some(first_path) = first_path else {
        return Ok(None);
    };
    Ok(Some(dash_localize_template_tokens(
        template,
        first_path,
        first_representation,
        first_number,
    )))
}

fn dash_localize_template_tokens(
    source_template: &str,
    mut local_template: String,
    first_representation: &DashRepresentation,
    first_number: u64,
) -> String {
    if let Some(token) = dash_template_number_token(source_template) {
        let raw_number = if let Some(width) = token.width {
            format!("{first_number:0width$}")
        } else {
            first_number.to_string()
        };
        if local_template.contains(&raw_number) {
            local_template = local_template.replacen(&raw_number, token.raw, 1);
        }
    }
    if source_template.contains("$RepresentationID$")
        && local_template.contains(&first_representation.id)
    {
        local_template = local_template.replacen(&first_representation.id, "$RepresentationID$", 1);
    }
    if source_template.contains("$Bandwidth$")
        && local_template.contains(&first_representation.bandwidth)
    {
        local_template = local_template.replacen(&first_representation.bandwidth, "$Bandwidth$", 1);
    }
    local_template
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DashNumberToken<'a> {
    raw: &'a str,
    width: Option<usize>,
}

fn dash_template_number_token(template: &str) -> Option<DashNumberToken<'_>> {
    let start = template.find("$Number")?;
    let rest = &template[start..];
    let end = rest[1..].find('$')? + 2;
    let raw = &rest[..end];
    let inner = &raw[1..raw.len().saturating_sub(1)];
    let width = inner
        .strip_prefix("Number%0")
        .and_then(|raw| raw.strip_suffix('d'))
        .and_then(|raw| raw.parse::<usize>().ok());
    Some(DashNumberToken { raw, width })
}

fn rewrite_xml_element_texts(body: &str, name: &str, replacement: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    let open = format!("<{name}");
    let close = format!("</{name}>");
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(open_end) = rest.find('>') else {
            out.push_str(rest);
            return out;
        };
        let head_end = open_end + 1;
        out.push_str(&rest[..head_end]);
        rest = &rest[head_end..];
        let Some(close_start) = rest.find(&close) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(replacement);
        out.push_str(&rest[close_start..close_start + close.len()]);
        rest = &rest[close_start + close.len()..];
    }
    out.push_str(rest);
    out
}

fn xml_escape_attr(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_tags(body: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
            if let Some(end) = rest.find('>') {
                rest = &rest[end + 1..];
                continue;
            }
            break;
        }
        let Some(end) = rest.find('>') else {
            break;
        };
        tags.push(rest[..end].trim().trim_end_matches('/').trim().to_string());
        rest = &rest[end + 1..];
    }
    tags
}

fn xml_tags_named(body: &str, name: &str) -> Vec<String> {
    xml_tags(body)
        .into_iter()
        .filter(|tag| {
            tag == name
                || tag
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.chars().next().is_some_and(char::is_whitespace))
        })
        .collect()
}

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    loop {
        let idx = rest.find(name)?;
        rest = &rest[idx + name.len()..];
        if !rest.trim_start().starts_with('=') {
            continue;
        }
        rest = rest.trim_start();
        rest = rest.strip_prefix('=')?.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        rest = &rest[quote.len_utf8()..];
        let end = rest.find(quote)?;
        return Some(xml_unescape(&rest[..end]));
    }
}

fn xml_element_texts(body: &str, name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = body;
    let open = format!("<{name}");
    let close = format!("</{name}>");
    while let Some(start) = rest.find(&open) {
        rest = &rest[start + open.len()..];
        let Some(open_end) = rest.find('>') else {
            break;
        };
        rest = &rest[open_end + 1..];
        let Some(close_start) = rest.find(&close) else {
            break;
        };
        values.push(xml_unescape(&rest[..close_start]));
        rest = &rest[close_start + close.len()..];
    }
    values
}

fn xml_unescape(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn safe_browser_download_filename(raw: &str) -> String {
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    let mut out = String::new();
    let mut last_dash = false;
    for ch in leaf.chars() {
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
        if out.len() >= 128 {
            break;
        }
    }
    let out = out.trim_matches(['.', '-', '_']);
    if out.is_empty() {
        "browser-media".to_string()
    } else {
        out.to_string()
    }
}

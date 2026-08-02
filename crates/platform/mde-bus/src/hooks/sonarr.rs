//! Sonarr / Radarr adapter — extracts template fields from
//! Servarr-family webhook payloads (BUS-3.5).
//!
//! Both Sonarr (TV) and Radarr (movies) ship webhooks under the
//! same payload shape (the "Connect" notification system); the
//! `eventType` field on the body tells you which event class
//! fired. They don't set a custom `X-` header to distinguish
//! themselves from each other — that's intentional in the
//! upstream design (the payload's `instanceName` carries the
//! provenance instead). So the adapter dispatches on
//! `body.eventType` rather than a header.
//!
//! Exposed template fields per event:
//!
//! | eventType    | fields                                              |
//! |-------------:|-----------------------------------------------------|
//! | `Download`   | `title`, `quality`, `size_human`, `instance`, `target` |
//! | `Grab`       | `title`, `quality`, `instance`, `release_title`     |
//! | `HealthIssue`| `instance`, `severity`, `message`                   |
//! | `Test`       | `instance`                                          |
//!
//! `title` resolves to either the Series title (Sonarr) or the
//! Movie title (Radarr); `target` to either the episode label
//! ("S01E03") or the year for movies — same template variable
//! names work against both Servarr flavors so operators can
//! share one rule set across both.

use std::collections::BTreeMap;

use serde_json::Value;

use super::matcher::Adapter;

/// The Sonarr/Radarr adapter — stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct SonarrAdapter;

impl Adapter for SonarrAdapter {
    fn extract(
        &self,
        _headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Option<(String, BTreeMap<String, String>)> {
        let event = body.get("eventType").and_then(Value::as_str)?.to_string();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();

        if let Some(instance) = body.get("instanceName").and_then(Value::as_str) {
            fields.insert("instance".to_string(), instance.to_string());
        }

        match event.as_str() {
            "Download" | "Grab" => {
                // Sonarr ships `series.title`; Radarr ships `movie.title`.
                // Whichever is present wins.
                let title = body
                    .pointer("/series/title")
                    .and_then(Value::as_str)
                    .or_else(|| body.pointer("/movie/title").and_then(Value::as_str));
                if let Some(t) = title {
                    fields.insert("title".to_string(), t.to_string());
                }

                // Quality: from the first `episodes[*].quality.quality.name`
                // (Sonarr) or from `release.quality` / `movieFile.quality`
                // (Radarr).
                let quality = body
                    .pointer("/episodeFile/quality")
                    .and_then(Value::as_str)
                    .or_else(|| body.pointer("/movieFile/quality").and_then(Value::as_str))
                    .or_else(|| body.pointer("/release/quality").and_then(Value::as_str));
                if let Some(q) = quality {
                    fields.insert("quality".to_string(), q.to_string());
                }

                // Episode label "S01E03" for Sonarr.
                if let Some(episodes) = body.get("episodes").and_then(Value::as_array) {
                    if let Some(first) = episodes.first() {
                        let season = first.get("seasonNumber").and_then(Value::as_i64);
                        let ep = first.get("episodeNumber").and_then(Value::as_i64);
                        if let (Some(s), Some(e)) = (season, ep) {
                            fields.insert("target".to_string(), format!("S{s:02}E{e:02}"));
                        }
                    }
                }
                // Year for Radarr — `movie.year`.
                if !fields.contains_key("target") {
                    if let Some(year) = body.pointer("/movie/year").and_then(Value::as_i64) {
                        fields.insert("target".to_string(), year.to_string());
                    }
                }

                // Human-readable size — `episodeFile.size` or `movieFile.size`.
                let size_bytes = body
                    .pointer("/episodeFile/size")
                    .and_then(Value::as_i64)
                    .or_else(|| body.pointer("/movieFile/size").and_then(Value::as_i64));
                if let Some(b) = size_bytes {
                    fields.insert("size_human".to_string(), humanize_bytes(b));
                }

                // Grab carries `release.releaseTitle`.
                if event == "Grab" {
                    if let Some(rt) = body
                        .pointer("/release/releaseTitle")
                        .and_then(Value::as_str)
                    {
                        fields.insert("release_title".to_string(), rt.to_string());
                    }
                }
            }
            "HealthIssue" => {
                if let Some(level) = body.get("level").and_then(Value::as_str) {
                    fields.insert("severity".to_string(), level.to_string());
                }
                if let Some(msg) = body.get("message").and_then(Value::as_str) {
                    fields.insert("message".to_string(), msg.to_string());
                }
            }
            "Test" => {
                // No additional fields beyond `instance`.
            }
            _ => {
                fields.insert("event".to_string(), event.clone());
            }
        }

        Some((event, fields))
    }
}

/// Render a byte count as a SI-suffixed human string. Sonarr and
/// Radarr both publish raw bytes; operators usually want
/// "3.4 GB" in their notification body.
fn humanize_bytes(b: i64) -> String {
    let bf = b as f64;
    let kb = 1024.0;
    let mb = kb * 1024.0;
    let gb = mb * 1024.0;
    let tb = gb * 1024.0;
    if bf >= tb {
        format!("{:.1} TB", bf / tb)
    } else if bf >= gb {
        format!("{:.1} GB", bf / gb)
    } else if bf >= mb {
        format!("{:.1} MB", bf / mb)
    } else if bf >= kb {
        format!("{:.1} KB", bf / kb)
    } else {
        format!("{b} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sonarr_download_extracts_series_quality_episode_size() {
        let body = json!({
            "eventType": "Download",
            "instanceName": "Sonarr",
            "series": {"title": "Severance"},
            "episodes": [{"seasonNumber": 1, "episodeNumber": 3}],
            "episodeFile": {"size": 3_758_096_384i64, "quality": "Bluray-1080p"},
        });
        let (event, fields) = SonarrAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "Download");
        assert_eq!(fields.get("title").map(String::as_str), Some("Severance"));
        assert_eq!(fields.get("target").map(String::as_str), Some("S01E03"));
        assert_eq!(
            fields.get("quality").map(String::as_str),
            Some("Bluray-1080p")
        );
        assert_eq!(fields.get("size_human").map(String::as_str), Some("3.5 GB"));
        assert_eq!(fields.get("instance").map(String::as_str), Some("Sonarr"));
    }

    #[test]
    fn radarr_download_extracts_movie_year_quality() {
        let body = json!({
            "eventType": "Download",
            "instanceName": "Radarr",
            "movie": {"title": "Dune Part Two", "year": 2024},
            "movieFile": {"size": 4_294_967_296i64, "quality": "WEBDL-2160p"},
        });
        let (event, fields) = SonarrAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "Download");
        assert_eq!(
            fields.get("title").map(String::as_str),
            Some("Dune Part Two")
        );
        assert_eq!(fields.get("target").map(String::as_str), Some("2024"));
        assert_eq!(
            fields.get("quality").map(String::as_str),
            Some("WEBDL-2160p")
        );
        assert_eq!(fields.get("size_human").map(String::as_str), Some("4.0 GB"));
    }

    #[test]
    fn grab_event_carries_release_title() {
        let body = json!({
            "eventType": "Grab",
            "instanceName": "Sonarr",
            "series": {"title": "Andor"},
            "episodes": [{"seasonNumber": 2, "episodeNumber": 1}],
            "release": {"quality": "WEBDL-1080p", "releaseTitle": "Andor.S02E01.1080p"},
        });
        let (event, fields) = SonarrAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "Grab");
        assert_eq!(
            fields.get("release_title").map(String::as_str),
            Some("Andor.S02E01.1080p")
        );
    }

    #[test]
    fn health_issue_extracts_severity_and_message() {
        let body = json!({
            "eventType": "HealthIssue",
            "instanceName": "Radarr",
            "level": "error",
            "message": "Disk space low on /mnt/media",
        });
        let (event, fields) = SonarrAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "HealthIssue");
        assert_eq!(fields.get("severity").map(String::as_str), Some("error"));
        assert_eq!(
            fields.get("message").map(String::as_str),
            Some("Disk space low on /mnt/media")
        );
    }

    #[test]
    fn missing_event_type_returns_none() {
        let body = json!({"instanceName": "Sonarr"});
        assert!(SonarrAdapter.extract(&BTreeMap::new(), &body).is_none());
    }

    #[test]
    fn humanize_covers_all_scales() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(512), "512 B");
        assert_eq!(humanize_bytes(2048), "2.0 KB");
        assert_eq!(humanize_bytes(5_242_880), "5.0 MB");
        assert_eq!(humanize_bytes(3_758_096_384), "3.5 GB");
        // 1.5 TB
        assert_eq!(humanize_bytes(1_649_267_441_664), "1.5 TB");
    }
}

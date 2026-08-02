//! `mde-bus publish` — write a message to a topic + forward to
//! the local ntfy broker.
//!
//! Three publish forms per the BUS-1.8 task body:
//!
//! 1. **Default-verb (positional body)**:
//!    `mde-bus publish fleet/sec 'hello'`
//! 2. **Verbose (flag body)**:
//!    `mde-bus publish fleet/sec --body 'hello'`
//! 3. **Piped (stdin body)**:
//!    `echo 'hello' | mde-bus publish fleet/sec`
//!
//! Resolution order: positional `body` → `--body` flag → stdin.
//! Whichever is non-empty wins; if all three are empty the
//! verb exits with a clear error.
//!
//! The publish path always writes through the per-peer
//! [`Persist`] index (BUS-1.4) FIRST so a transient broker
//! failure doesn't lose the message. The outbound ntfy POST
//! is best-effort: if it fails (broker down, pre-enrollment
//! peer, etc.) the verb still exits 0 with the message safely
//! recorded — operators can re-publish from the index later.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Args;

use crate::hooks::config::Priority;
use crate::persist::Persist;

/// CLI args for `mde-bus publish`.
#[derive(Args, Debug, Default)]
pub struct PublishArgs {
    /// Topic to publish to (e.g. `fleet/announce`).
    pub topic: String,
    /// Optional positional message body.
    pub body: Option<String>,
    /// Alternative body flag (verbose form).
    #[arg(long)]
    pub body_flag: Option<String>,
    /// Optional title — becomes ntfy's `X-Title` header.
    #[arg(long)]
    pub title: Option<String>,
    /// Priority — one of `min` / `default` / `high` / `urgent`.
    #[arg(long, default_value = "default")]
    pub priority: String,
    /// BUS-2.7 — action button(s) as `LABEL=URL` (repeatable, ≤5). The
    /// URL (typically `mde://…`) is dispatched via `mde-open` when the
    /// button is clicked in the notification UI. Malformed entries (no
    /// `=`) are skipped.
    #[arg(long = "action", value_name = "LABEL=URL")]
    pub actions: Vec<String>,
    /// BUS-2.7.d — ULID of the message this replies to (threaded reply).
    /// Persists as `reply_to` in the envelope; BUS-6.1 renders threads.
    #[arg(long = "reply-to", value_name = "ULID")]
    pub reply_to: Option<String>,
    /// Persist-only mode — skip the broker POST entirely. Useful
    /// for offline message recording or for tests.
    #[arg(long)]
    pub no_broker: bool,
    /// Override the bus-root directory (defaults to
    /// `<XDG_DATA_HOME>/mde/bus`). Mainly for tests.
    #[arg(long)]
    pub bus_root: Option<PathBuf>,
    /// Override the broker URL (defaults to reading the overlay
    /// IP publish file + `http://<ip>:8443`).
    #[arg(long)]
    pub broker_url: Option<String>,
    /// Emit the persisted message as a JSON object instead of the
    /// bare ULID. Suitable for piping to `jq` or other JSON-aware
    /// tooling that needs full envelope visibility.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Parse a priority string into the [`Priority`] enum.
fn parse_priority(s: &str) -> Result<Priority> {
    match s.to_ascii_lowercase().as_str() {
        "min" => Ok(Priority::Min),
        "default" => Ok(Priority::Default),
        "high" => Ok(Priority::High),
        "urgent" => Ok(Priority::Urgent),
        other => Err(anyhow!(
            "invalid priority: {other} (expected min/default/high/urgent)"
        )),
    }
}

/// Resolve the body from the three accepted sources.
///
/// Pure helper exposed for unit tests — the live CLI uses
/// [`read_stdin`] but tests pass a stub closure.
pub fn resolve_body<F: FnOnce() -> Result<String>>(
    positional: Option<&str>,
    flag: Option<&str>,
    stdin_reader: F,
) -> Result<String> {
    if let Some(b) = positional {
        if !b.is_empty() {
            return Ok(b.to_string());
        }
    }
    if let Some(b) = flag {
        if !b.is_empty() {
            return Ok(b.to_string());
        }
    }
    let stdin_body = stdin_reader()?;
    if stdin_body.trim().is_empty() {
        return Err(anyhow!(
            "no message body supplied (positional arg, --body flag, or piped stdin all empty)"
        ));
    }
    Ok(stdin_body)
}

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .context("read stdin")?;
    Ok(s)
}

/// Resolve the default bus root from the env (XDG fallback).
fn default_bus_root() -> Result<PathBuf> {
    crate::default_data_dir().ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --bus-root"))
}

/// Resolve the default broker URL by reading the published
/// overlay-IP file. Returns `None` (not an error) when the file
/// is missing — that means the peer isn't enrolled, and the
/// caller should treat the publish as persist-only.
fn default_broker_url() -> Option<String> {
    let path = crate::broker::DEFAULT_OVERLAY_IP_PATH;
    std::fs::read_to_string(path).ok().and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(format!("http://{t}:{}", crate::broker::DEFAULT_LISTEN_PORT))
        }
    })
}

/// Execute the publish verb.
pub async fn run(args: PublishArgs) -> Result<()> {
    let priority = parse_priority(&args.priority)?;
    let body = resolve_body(args.body.as_deref(), args.body_flag.as_deref(), read_stdin)?;

    let bus_root = match args.bus_root.clone() {
        Some(p) => p,
        None => default_bus_root()?,
    };
    let persist = Persist::open(bus_root).context("open persist")?;
    let actions = parse_actions(&args.actions);
    let stored = persist
        .write_full(
            &args.topic,
            priority,
            args.title.as_deref(),
            Some(&body),
            &actions,
            args.reply_to.as_deref(),
        )
        .with_context(|| format!("persist publish {} → {}", args.topic, args.topic))?;

    if args.json {
        let s = serde_json::to_string(&stored).with_context(|| "serialize stored message")?;
        println!("{s}");
    } else {
        println!("{}", stored.ulid);
    }

    if args.no_broker {
        return Ok(());
    }

    let broker_url = args.broker_url.clone().or_else(default_broker_url);
    let Some(broker_url) = broker_url else {
        // Persist-only is fine — the audit log + GFS replication
        // will surface this message to other peers once the
        // broker comes up.
        eprintln!("note: no broker URL available; message persisted (--no-broker behaviour)");
        return Ok(());
    };

    // BROKER-RESILIENCE-1 — the CLI publish path is what the alert sources
    // (selinux_monitor, kdc_host, mesh-alert, the FDO bridge, …) shell out;
    // a dead broker must fail FAST here too (connect + request timeouts),
    // never hang the caller. The shared constructor applies both.
    let client = crate::hooks::publisher::broker_client();
    // NOTIFY-DIST-3 — flatten the hierarchical bus topic to a valid ntfy topic
    // segment (slashes/spaces → `_`). The hooks publisher already did this, but
    // the CLI publish path (used by the alert sources: selinux_monitor, kdc_host,
    // mesh-alert, …) posted the raw `a/b/c` topic → ntfy 404 → every alert fell
    // back to persist-only with no real-time broker push.
    let url = format!(
        "{}/{}",
        broker_url.trim_end_matches('/'),
        crate::hooks::publisher::ntfy_topic(&args.topic)
    );
    let mut req = client.post(&url).body(body.clone());
    if let Some(t) = args.title.as_ref() {
        req = req.header("X-Title", t.as_str());
    }
    req = req.header("X-Priority", priority.ntfy_header());

    match req.send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => {
            eprintln!(
                "note: ntfy returned {} — message persisted at ULID {} for retry",
                resp.status(),
                stored.ulid
            );
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "note: broker unreachable ({e}); message persisted at ULID {} for retry",
                stored.ulid
            );
            Ok(())
        }
    }
}

/// BUS-2.7 — parse `LABEL=URL` action specs (from `--action`) into
/// [`crate::persist::Action`]s. Splits on the first `=`; entries without
/// a `=` are skipped (lenient — a malformed spec drops rather than aborts
/// the publish). Labels + URLs are trimmed.
pub(crate) fn parse_actions(specs: &[String]) -> Vec<crate::persist::Action> {
    specs
        .iter()
        .filter_map(|s| {
            s.split_once('=')
                .map(|(label, url)| crate::persist::Action {
                    label: label.trim().to_string(),
                    url: url.trim().to_string(),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_body_prefers_positional() {
        let body = resolve_body(Some("from-arg"), Some("from-flag"), || {
            Ok("from-stdin".to_string())
        })
        .unwrap();
        assert_eq!(body, "from-arg");
    }

    #[test]
    fn resolve_body_falls_to_flag_when_positional_empty() {
        let body =
            resolve_body(Some(""), Some("from-flag"), || Ok("from-stdin".to_string())).unwrap();
        assert_eq!(body, "from-flag");
    }

    #[test]
    fn resolve_body_falls_to_stdin_when_others_absent() {
        let body = resolve_body(None, None, || Ok("from-stdin".to_string())).unwrap();
        assert_eq!(body, "from-stdin");
    }

    #[test]
    fn resolve_body_errors_when_all_empty() {
        let r = resolve_body(None, Some(""), || Ok("   ".to_string()));
        assert!(r.is_err());
    }

    #[test]
    fn parse_actions_splits_label_url_and_skips_malformed() {
        let specs = vec![
            "Resolve=mde://files/resolve".to_string(),
            "  View = https://x/y  ".to_string(), // trimmed
            "no-equals-sign".to_string(),         // skipped (no '=')
        ];
        let actions = parse_actions(&specs);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].label, "Resolve");
        assert_eq!(actions[0].url, "mde://files/resolve");
        assert_eq!(actions[1].label, "View");
        assert_eq!(actions[1].url, "https://x/y");
        assert!(parse_actions(&[]).is_empty());
    }

    #[test]
    fn parse_priority_accepts_all_four_levels() {
        assert_eq!(parse_priority("min").unwrap(), Priority::Min);
        assert_eq!(parse_priority("default").unwrap(), Priority::Default);
        assert_eq!(parse_priority("HIGH").unwrap(), Priority::High);
        assert_eq!(parse_priority("urgent").unwrap(), Priority::Urgent);
    }

    #[test]
    fn parse_priority_rejects_unknown() {
        assert!(parse_priority("supercritical").is_err());
    }

    #[tokio::test]
    async fn publish_no_broker_writes_to_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let args = PublishArgs {
            topic: "test/x".to_string(),
            body: Some("hello".to_string()),
            body_flag: None,
            title: Some("title".to_string()),
            priority: "default".to_string(),
            no_broker: true,
            bus_root: Some(tmp.path().to_path_buf()),
            broker_url: None,
            json: false,
            actions: Vec::new(),
            reply_to: Some("01J0CLIPARENT".to_string()),
        };
        run(args).await.unwrap();
        // Verify Persist stored the message.
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        let rows = p.list_since("test/x", None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_deref(), Some("title"));
        assert_eq!(rows[0].body.as_deref(), Some("hello"));
        // BUS-2.7.d — reply_to lives in the on-disk JSON (not the SQLite
        // index); confirm the CLI threaded --reply-to through write_full.
        let raw = std::fs::read_to_string(tmp.path().join(&rows[0].file_path)).unwrap();
        let parsed: crate::persist::StoredMessage = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.reply_to.as_deref(), Some("01J0CLIPARENT"));
    }
}

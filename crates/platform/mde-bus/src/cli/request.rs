//! `mde-bus request` — fire an `action/<domain>/<verb>` command and
//! wait for its reply (EPIC-BUS-EXT-ACTION).
//!
//! Publishes to the action topic, then polls `reply/<request-ulid>`
//! until a responder answers or the timeout fires. Prints the reply
//! body on success; exits non-zero on timeout so scripts + pre-commit
//! hooks can gate on a responder actually answering.
//!
//! This is the operator-facing handle on the Bus RPC pattern the
//! `mde_bus::rpc` library exposes for in-process Rust callers.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Args;

use crate::persist::Persist;
use crate::rpc;

/// CLI args for `mde-bus request`.
#[derive(Args, Debug, Default)]
pub struct RequestArgs {
    /// Action topic — must be in the `action/<domain>/<verb>`
    /// namespace (e.g. `action/meshfs/rebalance`).
    pub action_topic: String,
    /// Optional request body (e.g. a gfid or a window address).
    pub body: Option<String>,
    /// Optional request title.
    #[arg(long)]
    pub title: Option<String>,
    /// Priority of the action message (min/default/high/urgent).
    #[arg(long, default_value = "default")]
    pub priority: String,
    /// Reply timeout in seconds (default 30 per the Q31 lock).
    #[arg(long, default_value_t = 30)]
    pub timeout_secs: u64,
    /// Emit the reply as a JSON object instead of just its body.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Override the bus-root directory (defaults to
    /// `<XDG_DATA_HOME>/mde/bus`). Mainly for tests.
    #[arg(long)]
    pub bus_root: Option<PathBuf>,
}

fn parse_priority(s: &str) -> Result<crate::hooks::config::Priority> {
    use crate::hooks::config::Priority;
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

fn resolve_bus_root(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(p);
    }
    crate::default_data_dir().ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --bus-root"))
}

/// Execute `mde-bus request`.
pub async fn run(args: RequestArgs) -> Result<()> {
    let priority = parse_priority(&args.priority)?;
    let bus_root = resolve_bus_root(args.bus_root)?;
    let persist = Persist::open(bus_root).context("open persist")?;
    let timeout = Duration::from_secs(args.timeout_secs);

    match rpc::request(
        &persist,
        &args.action_topic,
        priority,
        args.title.as_deref(),
        args.body.as_deref(),
        timeout,
    )
    .await
    {
        Ok(reply) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&reply).unwrap_or_else(|_| String::from("{}"))
                );
            } else {
                println!("{}", reply.body.as_deref().unwrap_or(""));
            }
            Ok(())
        }
        // Timeout exits non-zero so callers can detect "no responder".
        Err(e) => Err(anyhow!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_times_out_with_no_responder() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(RequestArgs {
            action_topic: "action/test/ping".to_string(),
            body: None,
            title: None,
            priority: "default".to_string(),
            timeout_secs: 0, // immediate deadline → one poll then timeout
            json: false,
            bus_root: Some(tmp.path().to_path_buf()),
        })
        .await;
        assert!(r.is_err(), "no responder → non-zero exit");
    }

    #[tokio::test]
    async fn request_rejects_bad_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(RequestArgs {
            action_topic: "action/test/ping".to_string(),
            body: None,
            title: None,
            priority: "banana".to_string(),
            timeout_secs: 1,
            json: false,
            bus_root: Some(tmp.path().to_path_buf()),
        })
        .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn request_rejects_non_action_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(RequestArgs {
            action_topic: "fleet/announce".to_string(),
            body: None,
            title: None,
            priority: "default".to_string(),
            timeout_secs: 1,
            json: false,
            bus_root: Some(tmp.path().to_path_buf()),
        })
        .await;
        assert!(r.is_err(), "non-action topic rejected");
    }
}

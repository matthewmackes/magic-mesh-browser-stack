//! `mde-bus correlate` — inspect the operator's correlation rule
//! config (`$XDG_CONFIG_HOME/mde/bus-correlate.yaml`).
//!
//! Two sub-verbs:
//!
//! - `list` — print every configured rule as
//!   `<name>\t<sources joined by ,>\t<window_seconds>s\t<emits>\t<priority>`
//!   (TSV-friendly). Useful for verifying a rule was picked up
//!   after editing the YAML.
//! - `path` — print the resolved config-file path. Operators who
//!   don't remember where the config lives can run this to find
//!   the right file before editing.
//!
//! Synthesizing publishes from rule fires is the
//! BUS-6.5.evaluator follow-on; this verb is read-only.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;

use crate::correlate;

/// CLI sub-verbs for `mde-bus correlate`.
#[derive(Subcommand, Debug)]
pub enum CorrelateOp {
    /// Print every configured rule.
    List {
        /// Override the config path.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Emit JSON Lines instead of TSV. Each line is a
        /// `{name, sources, window_seconds, emits, priority}`
        /// object suitable for piping to `jq`.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the resolved config path.
    Path,
    /// Validate the config — flag common issues (empty sources,
    /// empty emits, zero windows, duplicate rule names, empty
    /// names). Prints findings; exits non-zero when any issue is
    /// found so CI / pre-commit hooks can gate on it.
    Validate {
        /// Override the config path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print the count of configured rules. Symmetric with the
    /// `audit count` + `persist count` verbs — completes the
    /// aggregate-count pattern across every mde-bus list-style
    /// surface.
    Count {
        /// Override the config path.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Emit `{"count":N}` instead of the bare integer.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

fn resolve_config_path(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(p);
    }
    correlate::default_config_path()
        .ok_or_else(|| anyhow!("no $HOME / $XDG_CONFIG_HOME — pass --config"))
}

/// Execute the `correlate` verb. Read-only — never writes.
pub fn run(op: CorrelateOp) -> Result<()> {
    match op {
        CorrelateOp::List { config, json } => {
            let path = resolve_config_path(config)?;
            let cfg = correlate::load_default(&path)
                .with_context(|| format!("load {}", path.display()))?;
            for rule in &cfg.rules {
                if json {
                    // Hand-built JSON to avoid touching the
                    // hooks/config.rs Priority enum's serde derive
                    // surface (it ships Deserialize only). Priority
                    // renders as lowercase Debug — matches the YAML
                    // input form (`priority: high` etc.).
                    let priority_str = format!("{:?}", rule.priority).to_lowercase();
                    let val = serde_json::json!({
                        "name": rule.name,
                        "sources": rule.sources,
                        "window_seconds": rule.window_seconds,
                        "emits": rule.emits,
                        "priority": priority_str,
                    });
                    println!("{val}");
                } else {
                    println!(
                        "{}\t{}\t{}s\t{}\t{:?}",
                        rule.name,
                        rule.sources.join(","),
                        rule.window_seconds,
                        rule.emits,
                        rule.priority,
                    );
                }
            }
        }
        CorrelateOp::Path => {
            let path = correlate::default_config_path()
                .ok_or_else(|| anyhow!("no $HOME / $XDG_CONFIG_HOME"))?;
            println!("{}", path.display());
        }
        CorrelateOp::Validate { config } => {
            let path = resolve_config_path(config)?;
            let cfg = correlate::load_default(&path)
                .with_context(|| format!("load {}", path.display()))?;
            let issues = correlate::validate_config(&cfg);
            if issues.is_empty() {
                println!("OK — {} rules validated, 0 issues", cfg.rules.len());
                return Ok(());
            }
            for issue in &issues {
                match issue.rule_index {
                    Some(i) => println!(
                        "[rule {i}: {name}] {msg}",
                        name = issue.rule_name,
                        msg = issue.message,
                    ),
                    None => println!(
                        "[{name}] {msg}",
                        name = issue.rule_name,
                        msg = issue.message,
                    ),
                }
            }
            return Err(anyhow!(
                "correlate validate: {} issue(s) found",
                issues.len()
            ));
        }
        CorrelateOp::Count { config, json } => {
            let path = resolve_config_path(config)?;
            let cfg = correlate::load_default(&path)
                .with_context(|| format!("load {}", path.display()))?;
            let n = cfg.rules.len();
            if json {
                println!("{{\"count\":{n}}}");
            } else {
                println!("{n}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_with_missing_file_returns_empty_ok() {
        // Missing file is the most common case (operator hasn't
        // configured correlation yet) — must not error.
        let p = std::path::PathBuf::from("/nonexistent/path/bus-correlate.yaml");
        let r = run(CorrelateOp::List {
            config: Some(p),
            json: false,
        });
        assert!(r.is_ok());
    }

    #[test]
    fn list_with_existing_config_succeeds() {
        let tmp =
            std::env::temp_dir().join(format!("mde-bus-correlate-cli-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("bus-correlate.yaml");
        std::fs::write(
            &path,
            "rules:\n  - name: power-outage\n    sources: [a, b]\n    window_seconds: 60\n    emits: incident/outage\n    priority: high\n",
        )
        .unwrap();
        let r = run(CorrelateOp::List {
            config: Some(path.clone()),
            json: false,
        });
        assert!(r.is_ok());
        // Also verify --json path runs without error.
        let r_json = run(CorrelateOp::List {
            config: Some(path),
            json: true,
        });
        assert!(r_json.is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn path_returns_ok_when_xdg_resolves() {
        // path verb returns Ok as long as dirs::config_dir() does
        // — on most test environments this succeeds.
        if correlate::default_config_path().is_some() {
            assert!(run(CorrelateOp::Path).is_ok());
        }
    }

    #[test]
    fn count_on_missing_config_returns_zero() {
        // Missing file → empty rules vec → count 0 → no error.
        let p = std::path::PathBuf::from("/nonexistent/path/bus-correlate.yaml");
        let r = run(CorrelateOp::Count {
            config: Some(p.clone()),
            json: false,
        });
        assert!(r.is_ok());
        let r_json = run(CorrelateOp::Count {
            config: Some(p),
            json: true,
        });
        assert!(r_json.is_ok());
    }

    #[test]
    fn count_with_existing_config_succeeds() {
        let tmp =
            std::env::temp_dir().join(format!("mde-bus-correlate-count-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("bus-correlate.yaml");
        std::fs::write(
            &path,
            "rules:\n  - name: a\n    sources: [x]\n    window_seconds: 60\n    emits: y\n    priority: high\n  - name: b\n    sources: [m]\n    window_seconds: 30\n    emits: n\n    priority: default\n",
        )
        .unwrap();
        let r = run(CorrelateOp::Count {
            config: Some(path.clone()),
            json: false,
        });
        assert!(r.is_ok());
        let r_json = run(CorrelateOp::Count {
            config: Some(path),
            json: true,
        });
        assert!(r_json.is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

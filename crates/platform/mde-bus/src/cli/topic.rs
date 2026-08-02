//! `mde-bus topic` — list every known topic in the registry
//! or match against a wildcard pattern.
//!
//! Two sub-verbs:
//!
//! - `list` — print every seeded + dynamically-created topic
//!   as `<name>\t<priority>\t<description>` (TSV-friendly).
//! - `match <pattern>` — print topics matching an MQTT wildcard
//!   (`+` / `#`), useful for previewing a `tail` or `sub` glob.

use anyhow::Result;
use clap::Subcommand;

use crate::seed;
use crate::topic::Registry;

/// CLI sub-verbs for `mde-bus topic`.
#[derive(Subcommand, Debug)]
pub enum TopicOp {
    /// Print every known topic.
    List {
        /// Emit JSON Lines instead of TSV. Each line is a
        /// `{name, priority, description}` object suitable for
        /// piping to `jq`.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print topics matching the given pattern.
    Match {
        /// MQTT-style pattern (`+` single-level, `#` multi-level).
        pattern: String,
        /// Emit JSON Lines instead of plain text. Each line is a
        /// `{name, priority, description}` object so `jq` pipes
        /// can carry forward the registry metadata, not just the
        /// matched topic name.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the count of topics in the registry (optionally
    /// filtered by an MQTT-style pattern). Symmetric with the
    /// audit / persist / correlate / sub / mute count verbs.
    Count {
        /// Optional pattern to scope the count. None = total
        /// registered topic count.
        #[arg(long)]
        pattern: Option<String>,
        /// Emit `{"count":N}` instead of the bare integer.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// Build a registry pre-loaded with the 12 default topics. Used
/// by both `list` and `match` so they have something to enumerate
/// even when the daemon hasn't been started yet.
fn build_seeded_registry() -> Result<Registry> {
    let mut reg = Registry::new();
    seed::seed_defaults(&mut reg)?;
    Ok(reg)
}

/// Execute the `topic` verb.
pub fn run(op: TopicOp) -> Result<()> {
    let reg = build_seeded_registry()?;
    match op {
        TopicOp::List { json } => {
            for t in reg.iter() {
                if json {
                    let priority_str = format!("{:?}", t.priority_default).to_lowercase();
                    let val = serde_json::json!({
                        "name": t.name,
                        "priority": priority_str,
                        "description": t.description,
                    });
                    println!("{val}");
                } else {
                    println!("{}\t{:?}\t{}", t.name, t.priority_default, t.description);
                }
            }
        }
        TopicOp::Count { pattern, json } => {
            let n = if let Some(p) = pattern.as_deref() {
                reg.iter()
                    .filter(|t| crate::wildcard::matches(p, &t.name))
                    .count()
            } else {
                reg.iter().count()
            };
            if json {
                println!("{{\"count\":{n}}}");
            } else {
                println!("{n}");
            }
        }
        TopicOp::Match { pattern, json } => {
            for t in reg.iter() {
                if crate::wildcard::matches(&pattern, &t.name) {
                    if json {
                        let priority_str = format!("{:?}", t.priority_default).to_lowercase();
                        let val = serde_json::json!({
                            "name": t.name,
                            "priority": priority_str,
                            "description": t.description,
                        });
                        println!("{val}");
                    } else {
                        println!("{}", t.name);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_runs_without_error() {
        run(TopicOp::List { json: false }).unwrap();
        run(TopicOp::List { json: true }).unwrap();
    }

    #[test]
    fn seeded_registry_accepts_fresh_fedora_dotted_hostname() {
        let mut reg = Registry::new();
        seed::seed_defaults_with_hostname(&mut reg, "localhost.localdomain").unwrap();
        let names: Vec<&str> = reg.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"peer/localhost.localdomain/alerts"));
        assert!(names.contains(&"peer/localhost.localdomain/system"));
    }

    #[test]
    fn match_filters_by_pattern() {
        // Verify against the registry directly to avoid stdout
        // capture.
        let reg = build_seeded_registry().unwrap();
        let mut matched: Vec<&str> = reg
            .iter()
            .filter(|t| crate::wildcard::matches("mon/#", &t.name))
            .map(|t| t.name.as_str())
            .collect();
        matched.sort();
        assert!(matched.contains(&"mon/cpu"));
        assert!(matched.contains(&"mon/memory"));
        assert!(matched.contains(&"mon/disk"));
        assert!(matched.contains(&"mon/network"));
    }

    #[test]
    fn match_verb_runs_without_error() {
        run(TopicOp::Match {
            pattern: "mon/+".to_string(),
            json: false,
        })
        .unwrap();
    }

    #[test]
    fn count_verb_returns_total() {
        // No filter → registry total count (22+ default topics
        // per seed_defaults). Both dispatch paths should not panic.
        run(TopicOp::Count {
            pattern: None,
            json: false,
        })
        .unwrap();
        run(TopicOp::Count {
            pattern: None,
            json: true,
        })
        .unwrap();
    }

    #[test]
    fn count_verb_with_pattern_filters() {
        // Scoped count over the default registry: `mon/+` should
        // match the 4 seeded mon/* topics.
        run(TopicOp::Count {
            pattern: Some("mon/+".to_string()),
            json: false,
        })
        .unwrap();
        run(TopicOp::Count {
            pattern: Some("mon/+".to_string()),
            json: true,
        })
        .unwrap();
    }

    #[test]
    fn match_verb_json_path_runs() {
        // Exercise the JSON branch of the match dispatcher;
        // `mon/+` matches the four seeded mon/* topics, all of
        // which round-trip through the json! macro cleanly.
        run(TopicOp::Match {
            pattern: "mon/+".to_string(),
            json: true,
        })
        .unwrap();
    }
}

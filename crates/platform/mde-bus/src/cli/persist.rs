//! `mde-bus persist` — diagnostics on the per-peer SQLite index +
//! per-topic file tree (BUS-1.4 persistence layer).
//!
//! `verify` is read-only and never deletes / never rewrites. Operators looking
//! for corruption or out-of-sync index state run `verify`; CI gates can gate on
//! the nonzero exit when divergence is detected. `repair` is the explicit
//! write path: it only re-indexes valid Bus message files by default, and prunes
//! missing-file rows only when the operator passes an explicit flag.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;

use crate::persist::Persist;

/// CLI sub-verbs for `mde-bus persist`.
#[derive(Subcommand, Debug)]
pub enum PersistOp {
    /// Walk the persistence layer + flag divergence between the
    /// SQLite index and the file tree. Prints two lists:
    /// `files_without_rows` (the file tree has it but the index
    /// doesn't — likely external write or index corruption) +
    /// `rows_without_files` (the index has it but the file tree
    /// doesn't — likely external delete or retention bug). Exits
    /// nonzero when any divergence is found so CI can gate on it.
    Verify {
        /// Override the bus-root directory.
        #[arg(long)]
        bus_root: Option<PathBuf>,
        /// Emit a JSON `{files_without_rows, rows_without_files}`
        /// object instead of the human-readable summary. Useful
        /// for piping to `jq` from CI scripts.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Reconcile safe divergence between the authoritative file tree and the
    /// local SQLite index. Re-indexes valid file-only Bus messages without
    /// rewriting their JSON bodies. Missing-file rows are reported by default;
    /// pruning them requires `--prune-missing-rows`, which exports the rows to
    /// `.repair/` before deleting them from the index.
    Repair {
        /// Override the bus-root directory.
        #[arg(long)]
        bus_root: Option<PathBuf>,
        /// Report the repair plan without changing the index.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Explicitly remove rows whose authoritative JSON file is gone. The
        /// removed rows are first exported under `<bus-root>/.repair/`.
        #[arg(long, default_value_t = false)]
        prune_missing_rows: bool,
        /// Emit JSON instead of the human-readable summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the total number of indexed messages in the SQLite
    /// store. Read-only; symmetric with `audit count`. Useful for
    /// monitoring + dashboards ("how many bus messages persist
    /// right now?") + capacity planning ("at the current pace,
    /// when will we hit the retention quota?").
    Count {
        /// Override the bus-root directory.
        #[arg(long)]
        bus_root: Option<PathBuf>,
        /// Emit `{"count":N}` instead of the bare integer for
        /// jq-pipe symmetry with `audit count --json`.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

fn resolve_bus_root(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(p);
    }
    crate::default_data_dir().ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --bus-root"))
}

/// Execute the `persist` verb. Read-only.
pub fn run(op: PersistOp) -> Result<()> {
    match op {
        PersistOp::Verify { bus_root, json } => {
            let root = resolve_bus_root(bus_root)?;
            let p = Persist::open(root.clone())
                .with_context(|| format!("open persist at {}", root.display()))?;
            let report = p
                .detect_divergence()
                .with_context(|| format!("scan {}", root.display()))?;
            let clean = report.is_clean();
            if json {
                let val = serde_json::json!({
                    "files_without_rows": report.files_without_rows,
                    "rows_without_files": report.rows_without_files,
                    "invalid_message_files": report.invalid_message_files,
                    "ignored_non_message_files": report.ignored_non_message_files,
                });
                println!("{val}");
            } else if clean {
                println!("OK — persist clean (0 divergent rows, 0 orphan files)");
            } else {
                println!(
                    "DIVERGENT — {} file(s) without rows, {} row(s) without files, {} invalid message file(s)",
                    report.files_without_rows.len(),
                    report.rows_without_files.len(),
                    report.invalid_message_files.len(),
                );
                for f in &report.files_without_rows {
                    println!("  file-without-row: {f}");
                }
                for r in &report.rows_without_files {
                    println!("  row-without-file: {r}");
                }
                for f in &report.invalid_message_files {
                    println!("  invalid-message-file: {} ({})", f.path, f.reason);
                }
                for f in &report.ignored_non_message_files {
                    println!("  ignored-non-message-json: {f}");
                }
            }
            if !clean {
                return Err(anyhow!(
                    "persist verify: {} file(s) without rows, {} row(s) without files, {} invalid message file(s)",
                    report.files_without_rows.len(),
                    report.rows_without_files.len(),
                    report.invalid_message_files.len(),
                ));
            }
        }
        PersistOp::Repair {
            bus_root,
            dry_run,
            prune_missing_rows,
            json,
        } => {
            let root = resolve_bus_root(bus_root)?;
            let p = Persist::open(root.clone())
                .with_context(|| format!("open persist at {}", root.display()))?;
            let report = p
                .repair_divergence(dry_run, prune_missing_rows)
                .with_context(|| format!("repair {}", root.display()))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&report).context("serialize repair report")?
                );
            } else {
                if dry_run {
                    println!(
                        "DRY RUN — would reindex {} file(s), would prune {} missing row(s)",
                        report.files_would_reindex.len(),
                        report.rows_would_prune.len(),
                    );
                } else {
                    println!(
                        "REPAIR — reindexed {} file(s), pruned {} missing row(s)",
                        report.files_reindexed.len(),
                        report.rows_pruned.len(),
                    );
                }
                if let Some(path) = &report.missing_rows_export {
                    println!("  missing-row-export: {path}");
                }
                for f in &report.files_would_reindex {
                    println!("  would-reindex: {f}");
                }
                for f in &report.files_reindexed {
                    println!("  reindexed: {f}");
                }
                for r in &report.rows_without_files {
                    println!("  unresolved-row-without-file: {r}");
                }
                for r in &report.rows_would_prune {
                    println!("  would-prune-row: {r}");
                }
                for r in &report.rows_pruned {
                    println!("  pruned-row: {r}");
                }
                for f in &report.invalid_message_files {
                    println!("  invalid-message-file: {} ({})", f.path, f.reason);
                }
                for f in &report.ignored_non_message_files {
                    println!("  ignored-non-message-json: {f}");
                }
            }
            if report.has_unresolved() {
                return Err(anyhow!(
                    "persist repair: {} row(s) without files and {} invalid message file(s) still need operator action",
                    report.rows_without_files.len(),
                    report.invalid_message_files.len(),
                ));
            }
        }
        PersistOp::Count { bus_root, json } => {
            let root = resolve_bus_root(bus_root)?;
            let p = Persist::open(root.clone())
                .with_context(|| format!("open persist at {}", root.display()))?;
            let n = p
                .count()
                .with_context(|| format!("count messages at {}", root.display()))?;
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
    use crate::persist::StoredMessage;

    fn stored_message(topic: &str, ulid: &str, body: &str) -> StoredMessage {
        StoredMessage {
            ulid: ulid.to_owned(),
            topic: topic.to_owned(),
            priority: "default".to_owned(),
            title: None,
            body: Some(body.to_owned()),
            ts_unix_ms: 1_784_349_068_806,
            file_path: format!("{topic}/{ulid}.json"),
            actions: Vec::new(),
            reply_to: None,
        }
    }

    #[test]
    fn verify_empty_bus_root_returns_clean() {
        let tmp = tempfile::tempdir().unwrap();
        // Open Persist to seed the SQLite index file; then no
        // publishes happen → 0 rows + 0 files → clean.
        let _p = Persist::open(tmp.path().to_path_buf()).unwrap();
        drop(_p);
        let r = run(PersistOp::Verify {
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        });
        assert!(r.is_ok(), "empty persist should be clean: {r:?}");
    }

    #[test]
    fn verify_with_orphan_file_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        drop(p);
        // Drop a JSON file with no matching SQLite row.
        let ulid = "01KXSQW3G6PYVGSXPB2QEQW1XK";
        let msg = stored_message("orphan/topic", ulid, "orphan");
        let orphan_dir = tmp.path().join("orphan/topic");
        std::fs::create_dir_all(&orphan_dir).unwrap();
        std::fs::write(
            orphan_dir.join(format!("{ulid}.json")),
            serde_json::to_string_pretty(&msg).unwrap(),
        )
        .unwrap();
        let r = run(PersistOp::Verify {
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        });
        // Divergence detected → run returns Err.
        assert!(r.is_err());
    }

    #[test]
    fn verify_ignores_non_message_json() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        drop(p);
        std::fs::write(
            tmp.path().join("settings-nav.json"),
            r#"{"group":"mesh_system","section":"network"}"#,
        )
        .unwrap();
        let r = run(PersistOp::Verify {
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        });
        assert!(r.is_ok(), "plain app state JSON is not Bus divergence");
    }

    #[test]
    fn verify_json_path_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let _p = Persist::open(tmp.path().to_path_buf()).unwrap();
        drop(_p);
        let r = run(PersistOp::Verify {
            bus_root: Some(tmp.path().to_path_buf()),
            json: true,
        });
        assert!(r.is_ok());
    }

    #[test]
    fn count_on_empty_persist_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let _p = Persist::open(tmp.path().to_path_buf()).unwrap();
        drop(_p);
        // Both default + JSON paths exercise the verb. Output goes
        // to stdout; we just confirm dispatch + no error.
        let r = run(PersistOp::Count {
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        });
        assert!(r.is_ok());
        let r = run(PersistOp::Count {
            bus_root: Some(tmp.path().to_path_buf()),
            json: true,
        });
        assert!(r.is_ok());
    }

    #[test]
    fn count_returns_actual_message_count() {
        use crate::hooks::config::Priority;
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        for i in 0..3 {
            p.write("t/x", Priority::Default, None, Some(&i.to_string()))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // 6 = 3 t/x messages + 3 audit/<peer> records (one auto-emitted
        // per publish, EPIC-BUS-EXT-AUDIT-BUS).
        assert_eq!(p.count().unwrap(), 6);
        drop(p);
        // Dispatch the verb — count() is exercised, output to stdout.
        let r = run(PersistOp::Count {
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        });
        assert!(r.is_ok());
    }

    #[test]
    fn repair_reindexes_orphan_file_via_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        drop(p);
        let ulid = "01KXSQW3G6PYVGSXPB2QEQW1XM";
        let msg = stored_message("state/bookmarks/sync", ulid, "orphan");
        let orphan = tmp.path().join(&msg.file_path);
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, serde_json::to_string_pretty(&msg).unwrap()).unwrap();

        let r = run(PersistOp::Repair {
            bus_root: Some(tmp.path().to_path_buf()),
            dry_run: false,
            prune_missing_rows: false,
            json: false,
        });
        assert!(
            r.is_ok(),
            "repair should reindex the file-only message: {r:?}"
        );

        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        assert!(p.detect_divergence().unwrap().is_clean());
        assert_eq!(
            p.read_latest("state/bookmarks/sync").unwrap().unwrap().ulid,
            ulid
        );
    }

    #[test]
    fn repair_reports_unpruned_missing_rows_via_cli() {
        use crate::hooks::config::Priority;
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        let msg = p
            .write(
                "state/browser-custom-filter-rules-source/test",
                Priority::Default,
                None,
                Some("gone"),
            )
            .unwrap();
        std::fs::remove_file(tmp.path().join(&msg.file_path)).unwrap();
        drop(p);

        let r = run(PersistOp::Repair {
            bus_root: Some(tmp.path().to_path_buf()),
            dry_run: false,
            prune_missing_rows: false,
            json: false,
        });
        assert!(r.is_err(), "missing-file rows need explicit pruning");
    }
}

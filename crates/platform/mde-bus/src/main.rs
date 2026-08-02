//! `mde-bus` binary — entry point that parses CLI args and
//! dispatches to per-verb implementations in
//! [`mde_bus::cli::*`].
//!
//! Subcommands (one module each — see `crates/mde-bus/src/cli/`):
//!
//! - `mde-bus daemon` — long-lived bus daemon (broker + mDNS +
//!   subs watcher + webhooks listener). The default when no
//!   subcommand is given.
//! - `mde-bus publish` — publish a message to a topic.
//! - `mde-bus tail` — follow messages on a topic.
//! - `mde-bus sub` / `mde-bus mute` — manage per-peer
//!   subscription + mute patterns.
//! - `mde-bus history` — print stored messages on a topic.
//! - `mde-bus topic` — list / match topics in the registry.
//! - `mde-bus render` — render a Tera template against mesh
//!   variables.
//!
//! Use `RUST_LOG=mde_bus=debug,info` for verbose tracing.

use std::time::Duration;

use clap::Parser;

use mde_bus::cli::{Cli, Cmd};
use mde_bus::{
    broker, discovery, hooks, retention, seed, subs, template::Renderer, topic::Registry,
};

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("mde_bus=info,warn"));
    let fmt = tracing_subscriber::fmt::layer()
        .json()
        .with_target(true)
        .with_current_span(false);
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry().with(filter).with(fmt).init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Daemon) {
        Cmd::Daemon => run_daemon().await,
        Cmd::Render { template } => run_render(&template),
        Cmd::Publish(args) => mde_bus::cli::publish::run(args).await,
        Cmd::Request(args) => mde_bus::cli::request::run(args).await,
        Cmd::Tail(args) => mde_bus::cli::tail::run(args).await,
        Cmd::Sub { op } => mde_bus::cli::sub::run(op).await,
        Cmd::Mute { op } => mde_bus::cli::mute::run(op).await,
        Cmd::History(args) => mde_bus::cli::history::run(args).await,
        Cmd::Topic { op } => mde_bus::cli::topic::run(op),
        Cmd::Dnd { op } => mde_bus::cli::dnd::run(op).await,
        Cmd::Correlate { op } => mde_bus::cli::correlate::run(op),
        Cmd::Audit { op } => mde_bus::cli::audit::run(op),
        Cmd::Persist { op } => mde_bus::cli::persist::run(op),
        Cmd::Retention { op } => mde_bus::cli::retention::run(op),
        Cmd::Federation { op } => mde_bus::cli::federation::run(op),
    }
}

fn build_seeded_registry() -> anyhow::Result<Registry> {
    let mut reg = Registry::new();
    let created = seed::seed_defaults(&mut reg)?;
    tracing::info!(
        topics_seeded = created,
        topics_total = reg.len(),
        "registry initialised"
    );
    Ok(reg)
}

/// Exit code a redundant `mde-bus daemon` returns when another daemon already
/// owns the node-wide broker lock. The bus_supervisor treats this as "a broker
/// is healthy" and backs off to a slow poll instead of a tight respawn — so a
/// duplicate spawn can never crash-loop into a pile of zombies (the .13 wedge).
pub const EXIT_ALREADY_RUNNING: i32 = 3;

/// Claim the node-wide singleton lock via a Linux abstract-namespace unix
/// socket. The kernel releases the name automatically when this process exits,
/// so there is no stale lockfile to clean up after a crash. Returns the bound
/// listener (held for the daemon's lifetime) or `None` if another daemon holds
/// it. Abstract sockets are per-network-namespace, giving exactly one broker
/// per node — which is correct: the broker is node-level (mDNS + overlay
/// discovery), not per-user.
#[must_use]
fn acquire_singleton_lock() -> Option<std::os::unix::net::UnixListener> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let addr = SocketAddr::from_abstract_name(b"mde-bus-broker").ok()?;
    UnixListener::bind_addr(&addr).ok()
}

async fn run_daemon() -> anyhow::Result<()> {
    // Singleton guard: refuse to start a second broker. Held for the daemon's
    // whole lifetime (dropped on return) so the lock tracks liveness exactly.
    let _singleton = match acquire_singleton_lock() {
        Some(listener) => listener,
        None => {
            tracing::info!(
                "another mde-bus daemon already owns the broker lock; exiting cleanly \
                 (a node runs exactly one broker)"
            );
            std::process::exit(EXIT_ALREADY_RUNNING);
        }
    };
    let reg = build_seeded_registry()?;
    // BUS-1.2 — try to spawn the ntfy broker. Missing prereqs
    // (pre-enrollment peer, ntfy not installed, template not
    // shipped) are non-fatal: the daemon keeps idling and the
    // outer mackesd::bus_supervisor will respawn us on its next
    // restart cycle when prereqs land.
    let broker_cfg = broker::BrokerConfig::default();
    let broker_outcome = broker::start_if_ready(&broker_cfg).await?;
    // BROKER-RESILIENCE-1 — track whether the broker was SKIPPED (known absent)
    // distinctly from whether we have an overlay IP. A skip means "the local
    // ntfy isn't up" — but the webhook listener can still bind on the overlay
    // IP (when one exists) and ingest + persist webhooks; it just must run
    // persist-only (no outbound POST to the dead broker). Wiring the skip into
    // the listener's persist-only mode is what makes "non-fatal" actually true:
    // without it the listener either didn't start at all (lost ingest) or kept
    // POSTing to `:8443`, burning a connect-timeout per request and growing the
    // spool on retry — the live watchdog-starving wedge (diagnosed 2026-06-25).
    let (mut broker_child, overlay_ip_for_discovery, broker_skipped) = match broker_outcome {
        broker::BrokerOutcome::Running { child, overlay_ip } => {
            tracing::info!(
                topics = reg.len(),
                overlay_ip = %overlay_ip,
                "mackes-bus daemon ready (broker live); awaiting shutdown"
            );
            (Some(child), Some(overlay_ip), false)
        }
        broker::BrokerOutcome::Skipped(reason) => {
            // Re-evaluate prereqs so we can recover the overlay IP even on a
            // skip that ISN'T about the IP (e.g. `NtfyMissing` / template
            // missing): the listener can then still bind + persist there.
            let overlay_ip = match broker::evaluate_prereqs(&broker_cfg) {
                broker::Prereqs::Ready { overlay_ip } => Some(overlay_ip),
                broker::Prereqs::Skip(_) => None,
            };
            tracing::info!(
                topics = reg.len(),
                skip_reason = %reason,
                overlay_ip = ?overlay_ip,
                "mackes-bus daemon ready (broker skipped — non-fatal, persist-only); awaiting shutdown"
            );
            (None, overlay_ip, true)
        }
    };

    // BUS-1.3 — zeroconf discovery. Register `_mackes-bus._tcp.local.`
    // and browse for peers. Only run when the broker is live so we
    // don't advertise a port nothing's listening on. Missing overlay
    // IP or mdns-sd init failure logs the skip and continues.
    //
    // BROKER-RESILIENCE-1 — we now recover the overlay IP even on a broker
    // SKIP (so the listener can bind persist-only), but discovery must STILL
    // gate on the broker being LIVE: advertising `:8443` when nothing listens
    // there would point peers at a dead port. `broker_skipped` → `None` here.
    let discovery_overlay_ip = if broker_skipped {
        None
    } else {
        overlay_ip_for_discovery.clone()
    };
    let discovery_handle: Option<discovery::DiscoveryHandle> = match discovery_overlay_ip
        .as_deref()
        .map(str::parse::<std::net::IpAddr>)
    {
        Some(Ok(ip_addr)) => {
            let instance_name = hostname_for_discovery();
            let cfg = discovery::DiscoveryConfig::new(instance_name, ip_addr);
            let registry = discovery::PeerRegistry::new();
            match discovery::DiscoveryHandle::start(&cfg, registry) {
                Ok(handle) => {
                    tracing::info!(
                        target: "mde_bus::discovery",
                        "mDNS service active"
                    );
                    Some(handle)
                }
                Err(reason) => {
                    tracing::info!(
                        target: "mde_bus::discovery",
                        skip_reason = %reason,
                        "mDNS registration skipped — non-fatal"
                    );
                    None
                }
            }
        }
        Some(Err(e)) => {
            tracing::warn!(
                target: "mde_bus::discovery",
                error = %e,
                raw = ?discovery_overlay_ip,
                "overlay IP failed to parse; skipping mDNS registration"
            );
            None
        }
        None => None,
    };

    // BUS-1.7 — subscription manifest watcher. Polls the per-peer
    // subs.yaml every 100ms; on change re-parses + broadcasts the
    // new manifest via a `tokio::sync::watch` channel that future
    // delivery filters (BUS-1.8 CLI + BUS-4 webhooks) subscribe to.
    // Pre-enrollment peers + missing-template paths log + continue
    // with in-memory defaults.
    //
    // The shutdown sender is held in this function's scope so it
    // drops naturally when run_daemon returns — that triggers the
    // watcher's shutdown.changed() Err arm + clean exit.
    let (_subs_shutdown_tx, _subs_watcher_task) = match subs::default_per_peer_path() {
        Some(per_peer) => {
            let template = std::path::PathBuf::from(subs::DEFAULT_TEMPLATE_PATH);
            let initial_body = match subs::load_or_seed(&per_peer, &template) {
                Ok(body) => body,
                Err(e) => {
                    tracing::info!(
                        target: "mde_bus::subs",
                        error = %e,
                        "subs.yaml seed skipped — running with in-memory defaults"
                    );
                    String::new()
                }
            };
            let mut watcher = subs::SubsWatcher::new(per_peer, &initial_body);
            tracing::info!(
                target: "mde_bus::subs",
                topics = ?watcher.current().topics,
                "subs manifest loaded"
            );
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(async move {
                watcher.run(shutdown_rx).await;
            });
            (Some(shutdown_tx), Some(task))
        }
        None => {
            tracing::info!(
                target: "mde_bus::subs",
                skip_reason = %subs::SubsSkipReason::NoDataDir,
                "subs manifest watcher skipped — no XDG data home"
            );
            (None, None)
        }
    };
    // BUS-6.5.evaluator — load the operator's correlation rules +
    // spawn the evaluator loop. Each tick polls the persist index
    // for new messages on every rule source topic, observes them
    // into a per-topic sliding window, and shells out a synthesized
    // `mde-bus publish` for every rule whose sources all fired
    // within the window (cooldown-gated by window_seconds). An
    // invalid config logs + skips (no loop); a config with zero
    // rules spawns a loop that idles until shutdown.
    //
    // The shutdown sender is held in run_daemon's scope so it drops
    // (triggering the loop's clean exit) when the daemon returns.
    let (_correlate_shutdown_tx, _correlate_task) = match (
        mde_bus::correlate::default_config_path(),
        mde_bus::default_data_dir(),
    ) {
        (Some(path), Some(bus_root)) => match mde_bus::correlate::load_default(&path) {
            Ok(cfg) => {
                tracing::info!(
                    target: "mde_bus::correlate",
                    rules = cfg.rules.len(),
                    path = %path.display(),
                    "correlation rules loaded — evaluator loop starting"
                );
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(async move {
                    mde_bus::correlate::run_evaluator_loop(
                        cfg,
                        bus_root,
                        mde_bus::correlate::DEFAULT_EVAL_INTERVAL,
                        shutdown_rx,
                    )
                    .await;
                });
                (Some(shutdown_tx), Some(task))
            }
            Err(e) => {
                tracing::warn!(
                    target: "mde_bus::correlate",
                    error = %e,
                    "correlation config invalid — evaluator skipped"
                );
                (None, None)
            }
        },
        _ => {
            tracing::info!(
                target: "mde_bus::correlate",
                "correlation evaluator skipped — no XDG config/data dir"
            );
            (None, None)
        }
    };
    // BUS-2.8.watcher — mtime-poll watcher for <bus_root>/dnd.yaml.
    // Broadcasts DndState changes through a tokio::sync::watch
    // channel so subscribers (hook handler, future BUS-2.x display
    // surfaces) read the cached state without per-fire file IO.
    // Skip when no XDG data dir exists (pre-enrollment peer).
    let (_dnd_shutdown_tx, _dnd_watcher_task) = match mde_bus::default_data_dir() {
        Some(bus_root) => {
            let mut watcher = mde_bus::dnd::DndWatcher::new(bus_root);
            tracing::info!(
                target: "mde_bus::dnd",
                active = watcher.current().active,
                "dnd state watcher started"
            );
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(async move {
                watcher.run(shutdown_rx).await;
            });
            (Some(shutdown_tx), Some(task))
        }
        None => {
            tracing::info!(
                target: "mde_bus::dnd",
                "dnd state watcher skipped — no XDG data home"
            );
            (None, None)
        }
    };
    // BUS-3.1 + BUS-3.2 — webhook ingress HTTP listener. Binds on
    // <overlay_ip>:8444 only; bind-scope is the auth boundary
    // (kernel rejects underlay connects). Skip reasons (no
    // overlay IP, bind failed) log + continue — the outer
    // supervisor re-evaluates on its next tick.
    //
    // The shutdown sender is held in this function's scope so it
    // drops naturally when run_daemon returns — that triggers the
    // axum graceful-shutdown path.
    let (_hooks_shutdown_tx, _hooks_task, _hooks_local_addr) = match overlay_ip_for_discovery
        .as_deref()
        .map(str::parse::<std::net::IpAddr>)
    {
        Some(Ok(ip_addr)) => {
            // BROKER-RESILIENCE-1 — when the broker was skipped (known absent),
            // bind the listener persist-only: it ingests + persists webhooks but
            // never attempts the doomed outbound POST to the down `:8443`. When
            // the broker is live, forward to it as normal.
            let cfg = if broker_skipped {
                hooks::server::ListenerConfig::persist_only_for_overlay_ip(&ip_addr.to_string())
            } else {
                hooks::server::ListenerConfig::for_overlay_ip(&ip_addr.to_string())
            };
            match hooks::run_listener(ip_addr, cfg).await? {
                hooks::ListenerOutcome::Running {
                    task,
                    shutdown_tx,
                    local_addr,
                } => {
                    tracing::info!(
                        target: "mde_bus::hooks",
                        local_addr = %local_addr,
                        persist_only = broker_skipped,
                        "webhook listener active"
                    );
                    (Some(shutdown_tx), Some(task), Some(local_addr))
                }
                hooks::ListenerOutcome::Skipped(reason) => {
                    tracing::info!(
                        target: "mde_bus::hooks",
                        skip_reason = %reason,
                        "webhook listener skipped — non-fatal"
                    );
                    (None, None, None)
                }
            }
        }
        Some(Err(_)) | None => {
            tracing::info!(
                target: "mde_bus::hooks",
                "webhook listener skipped — no overlay IP available yet"
            );
            (None, None, None)
        }
    };

    // BUS-1.9 — retention loop. Spawns a tokio task that runs
    // one GC pass every hour: walks the SQLite index by
    // ts_unix_ms, deletes messages past their priority's TTL,
    // and publishes a bus/sys/quota warning when the soft
    // quota is exceeded. Skipped when bus_root resolution
    // fails (pre-XDG environment).
    let (_retention_shutdown_tx, _retention_task) = match mde_bus::default_data_dir() {
        Some(bus_root) => {
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let policy = retention::RetentionPolicy::default();
            let bus_root_clone = bus_root.clone();
            let task = tokio::spawn(async move {
                retention::run_loop(
                    policy,
                    bus_root_clone,
                    retention::DEFAULT_PASS_INTERVAL,
                    shutdown_rx,
                )
                .await;
            });
            tracing::info!(
                target: "mde_bus::retention",
                bus_root = %bus_root.display(),
                "retention loop active (hourly passes)"
            );
            (Some(shutdown_tx), Some(task))
        }
        None => {
            tracing::info!(
                target: "mde_bus::retention",
                "retention loop skipped — no XDG data home"
            );
            (None, None)
        }
    };

    // Heartbeat tick — every 60s log a single line so operators can
    // see the daemon is alive in `journalctl -u mde-bus`.
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        // When the broker is running, also wait on its exit — if
        // ntfy crashes we propagate the exit upward so the outer
        // mackesd supervisor restarts us with fresh prereq checks.
        let broker_wait: std::pin::Pin<
            Box<dyn std::future::Future<Output = std::io::Result<std::process::ExitStatus>> + Send>,
        > = if let Some(c) = broker_child.as_mut() {
            Box::pin(c.wait())
        } else {
            // Pending future so the select! arm is silent when no
            // broker is running.
            Box::pin(std::future::pending())
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received; shutting down");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received; shutting down");
                break;
            }
            status = broker_wait => {
                match status {
                    Ok(s) => tracing::warn!(
                        exit_code = ?s.code(),
                        "ntfy broker exited; mde-bus shutting down so the outer supervisor can respawn"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "wait() on ntfy broker failed; shutting down"
                    ),
                }
                broker_child = None;
                break;
            }
            _ = ticker.tick() => {
                let broker_state = if broker_child.is_some() { "live" } else { "skipped" };
                tracing::info!(topics = reg.len(), broker = broker_state, "heartbeat");
            }
        }
    }
    // Best-effort terminate the child on shutdown so we don't leak
    // an orphan ntfy process. `kill_on_drop` handles it on drop too,
    // but explicit is friendlier in logs.
    if let Some(mut child) = broker_child {
        tracing::info!("terminating ntfy broker child");
        let _ = child.kill().await;
    }
    // BUS-1.3 — unregister the mDNS service so peers see us drop in
    // real time, not after the cache TTL expires.
    if let Some(handle) = discovery_handle {
        tracing::info!("unregistering mDNS service");
        handle.shutdown();
    }
    Ok(())
}

/// Resolve a friendly instance name for this peer's mDNS
/// announcement. Honors `$MDE_BUS_INSTANCE` (tests/scripts), then
/// `$HOSTNAME` (commonly set by the shell), then reads
/// `/proc/sys/kernel/hostname` (kernel-owned source of truth),
/// then falls back to the stable string `"mde-bus"`.
fn hostname_for_discovery() -> String {
    for var in ["MDE_BUS_INSTANCE", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if let Ok(body) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "mde-bus".to_string()
}

fn run_render(template: &str) -> anyhow::Result<()> {
    let r = Renderer::new();
    let out = r.render(template)?;
    println!("{out}");
    Ok(())
}

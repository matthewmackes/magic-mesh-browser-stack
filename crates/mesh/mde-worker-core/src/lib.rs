//! Shared worker-pool contract for the `mackesd` control-plane daemon.
//!
//! `mackesd`'s in-process supervisor (v2.0.0 Phase A, locked 2026-05-19) folds
//! every former standalone daemon into one process, driving each as a
//! supervised [`Worker`] task. This crate holds the two types every worker
//! body touches:
//!
//! * [`Worker`] — the trait the supervisor stores as `Box<dyn Worker>`
//!   (`async_trait`, because native async-fn-in-trait isn't object-safe).
//! * [`ShutdownToken`] — the stop signal the supervisor hands each worker so
//!   its body can exit promptly on shutdown.
//!
//! They live here (rather than inside the `mackesd` bin crate) so worker
//! implementations can be factored into their own crates — e.g. the per-seat
//! browser workers in `mde-browser-workers` — without a circular dependency
//! back on the daemon. `mackesd`'s `workers` module re-exports both types, so
//! every in-crate worker keeps referencing `super::{Worker, ShutdownToken}`
//! unchanged.

#![forbid(unsafe_code)]

use tokio::sync::watch;

/// Shutdown signal handed to every worker. Workers should `select!`
/// on the underlying `watch::Receiver` so they exit promptly when
/// the supervisor requests stop. Cloning is cheap (it's a watch
/// receiver under the hood).
#[derive(Clone, Debug)]
pub struct ShutdownToken {
    rx: watch::Receiver<bool>,
}

impl ShutdownToken {
    /// Construct a token from a raw watch receiver.
    ///
    /// The supervisor's `Supervisor::token` is the normal public surface; this
    /// constructor lets the supervisor build a token from its shutdown channel
    /// and lets sibling worker modules build one from a freshly-paired
    /// sender/receiver pair in their unit tests.
    #[must_use]
    pub fn from_receiver(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }

    /// `true` once shutdown has been requested. Workers should poll
    /// or `await` on [`Self::wait`] for prompt notification.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        *self.rx.borrow()
    }

    /// Async wait for shutdown. Resolves the first time the
    /// supervisor flips the flag to `true`. Returns immediately if
    /// shutdown was already requested.
    pub async fn wait(&mut self) {
        if self.is_shutdown() {
            return;
        }
        // `changed()` errors only when the sender is dropped — at
        // which point we're shutting down anyway, so treat it as
        // shutdown-requested.
        let _ = self.rx.changed().await;
    }
}

/// Every worker registered with the supervisor implements this
/// trait. The trait is `async_trait` because the supervisor stores
/// `Box<dyn Worker>`, which native async-fn-in-trait doesn't yet
/// support.
#[async_trait::async_trait]
pub trait Worker: Send + 'static {
    /// Short, stable identifier used in logs + `mackesd healthz`
    /// output. Should be `kebab-case`/`snake_case` and match the matching
    /// worker module name (e.g. `clipboard_sync`, `mdns`,
    /// `notifications-server`).
    fn name(&self) -> &'static str;

    /// Body of the worker. Runs on the tokio runtime until
    /// `shutdown.wait().await` resolves OR the body returns. Errors
    /// returned here surface to the supervisor's restart logic.
    async fn run(&mut self, shutdown: ShutdownToken) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_reports_and_waits_for_shutdown() {
        let (tx, rx) = watch::channel(false);
        let mut token = ShutdownToken::from_receiver(rx);
        assert!(!token.is_shutdown());
        tx.send(true).expect("send shutdown");
        // Already flipped — wait() returns immediately.
        token.wait().await;
        assert!(token.is_shutdown());
    }

    #[tokio::test]
    async fn wait_resolves_when_sender_dropped() {
        let (tx, rx) = watch::channel(false);
        let mut token = ShutdownToken::from_receiver(rx);
        drop(tx);
        // Sender gone — treated as shutdown-requested, so this resolves.
        token.wait().await;
    }

    struct Noop;

    #[async_trait::async_trait]
    impl Worker for Noop {
        fn name(&self) -> &'static str {
            "noop"
        }
        async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
            shutdown.wait().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn worker_trait_is_object_safe_and_runs() {
        let (tx, rx) = watch::channel(false);
        let mut w: Box<dyn Worker> = Box::new(Noop);
        assert_eq!(w.name(), "noop");
        tx.send(true).expect("send");
        w.run(ShutdownToken::from_receiver(rx)).await.expect("run");
    }
}

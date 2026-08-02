//! BUS-3 — webhook ingress + YAML transform-rules engine.
//!
//! The Mackes Bus accepts inbound webhooks from external services
//! (GitHub, Gitea, Sonarr/Radarr, NUT, Home Assistant, generic JSON
//! posters) on the Nebula overlay IP, port 8444. A thin Rust shim
//! consumes every request, dispatches by adapter, extracts fields
//! per-adapter (Rust-native because each source has its own
//! payload shape), then renders user-defined templates from the
//! per-peer `~/.config/mde/bus-hooks.yaml` to produce the final
//! topic + title + body + priority. The publish is forwarded to
//! the local ntfy broker started by [`crate::broker`].
//!
//! Per `docs/design/v6.x-mackes-bus.md` §7:
//!
//! - **Authentication**: Nebula source-IP only — the listener
//!   binds on the overlay IP, so the kernel itself drops underlay
//!   connections. No tokens, no signatures.
//! - **Transport encryption**: none — Nebula provides it. Plain
//!   HTTP from the external poster, plain HTTP to ntfy.
//! - **Six built-in adapters**: github, gitea, sonarr, nut,
//!   home_assistant, generic.
//!
//! Sub-module layout (BUS-3.1 + BUS-3.2 scope):
//!
//! - [`config`] — top-level YAML schema (`HooksConfig` ↔
//!   `bus-hooks.yaml`) parsed via serde_yaml.
//! - [`matcher`] — per-request: pick adapter, run extractor, walk
//!   rule list, render templates.
//! - [`publisher`] — outbound HTTP POST to the local ntfy broker.
//! - [`server`] — axum HTTP listener bound to `<overlay_ip>:8444`.
//! - [`github`] — first built-in adapter (BUS-3.2 sample + BUS-3.3
//!   full event coverage land here).
//!
//! Each later BUS-3.N adapter adds one file to this module.

pub mod config;
pub mod generic;
pub mod gitea;
pub mod github;
pub mod home_assistant;
pub mod matcher;
pub mod nut;
pub mod publisher;
pub mod server;
pub mod sonarr;

pub use config::{AdapterConfig, HooksConfig, Match, Priority, PublishSpec, Rule};
pub use matcher::{match_request, RenderedPublish};
pub use publisher::{publish_to_ntfy, PublisherError};
pub use server::{run_listener, ListenerOutcome, ListenerSkipReason};

/// Default port the webhook ingress listens on. Matches the
/// `BUS-3.1` task body's `https://$peer:8444/<topic>` URL form;
/// transport is plain HTTP per design-doc §11 + §7 encryption
/// lock (Nebula is the security boundary, not TLS).
pub const DEFAULT_LISTEN_PORT: u16 = 8444;

/// Default path of the per-peer hooks config. Operators edit this
/// file; on next request the server re-reads it. The seed copy is
/// shipped at `/usr/share/mde/bus/hooks.yaml.tmpl` (data/bus/) and
/// installed by the spec.
pub const DEFAULT_CONFIG_PATH: &str = "~/.config/mde/bus-hooks.yaml";

/// Default path of the seed template shipped with the RPM.
pub const DEFAULT_TEMPLATE_PATH: &str = "/usr/share/mde/bus/hooks.yaml.tmpl";

/// Resolve the per-peer config path against `$HOME` (or
/// `$XDG_CONFIG_HOME` when set). Returns `None` when neither env
/// var is set — callers should treat that as
/// [`ListenerSkipReason::NoConfigPath`] and skip the listener
/// spawn, same shape the broker uses for missing prereqs.
#[must_use]
pub fn default_config_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("mde").join("bus-hooks.yaml"))
}

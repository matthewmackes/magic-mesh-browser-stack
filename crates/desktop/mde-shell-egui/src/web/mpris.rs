//! Browser-owned freedesktop MPRIS surface.
//!
//! This is standard FDO D-Bus interop only: Browser state and control remain on
//! `mde-bus`. The shell owns `org.mpris.MediaPlayer2.mde-browser`, reads the
//! retained `state/browser-media/<node>` mirror, and publishes transport methods
//! back to `action/browser/media-control/<node>` so Browser, KDC, and desktop
//! media controllers share one behavior path.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::missing_const_for_fn,
    clippy::module_name_repetitions,
    clippy::unused_async,
    clippy::unused_self,
    clippy::used_underscore_binding
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;
use mde_web_preview_client::MediaTransportAction;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use super::wire::{
    browser_media_control_body, browser_media_control_topic, browser_media_status_topic,
};

/// The Browser MPRIS well-known bus name.
pub(crate) const BUS_NAME: &str = "org.mpris.MediaPlayer2.mde-browser";
/// The standard MPRIS object path.
pub(crate) const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
const VOLUME_STEP_PERCENT: i64 = 5;
const MAX_VOLUME_STEPS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PlaybackState {
    #[default]
    Idle,
    Paused,
    Playing,
}

impl PlaybackState {
    fn from_wire(value: &str) -> Option<Self> {
        match value.trim() {
            "idle" => Some(Self::Idle),
            "paused" => Some(Self::Paused),
            "playing" => Some(Self::Playing),
            _ => None,
        }
    }

    fn as_mpris(self) -> &'static str {
        match self {
            Self::Idle => "Stopped",
            Self::Paused => "Paused",
            Self::Playing => "Playing",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BrowserMprisMetadata {
    title: String,
    artist: String,
    album: String,
    artwork_url: String,
    source_url: String,
    duration_ms: u64,
    position_ms: u64,
    volume_percent: Option<u64>,
}

impl BrowserMprisMetadata {
    fn from_value(value: &serde_json::Value) -> Self {
        Self {
            title: status_str(value, "title", 160).unwrap_or_default(),
            artist: status_str(value, "artist", 160).unwrap_or_default(),
            album: status_str(value, "album", 160).unwrap_or_default(),
            artwork_url: status_str(value, "artwork_url", 2048).unwrap_or_default(),
            source_url: status_str(value, "source_url", 2048).unwrap_or_default(),
            duration_ms: value
                .get("duration_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
            position_ms: value
                .get("position_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
            volume_percent: value
                .get("volume_percent")
                .and_then(serde_json::Value::as_u64)
                .map(|percent| percent.min(100)),
        }
    }

    fn has_content(&self) -> bool {
        !self.title.is_empty()
            || !self.artist.is_empty()
            || !self.album.is_empty()
            || !self.source_url.is_empty()
            || self.duration_ms > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BrowserMprisStatus {
    state: PlaybackState,
    tab_id: Option<u64>,
    url: String,
    page_title: String,
    label: String,
    metadata: BrowserMprisMetadata,
    updated_ms: u64,
}

impl BrowserMprisStatus {
    fn has_track(&self) -> bool {
        self.state != PlaybackState::Idle || self.tab_id.is_some() || self.metadata.has_content()
    }

    fn title(&self) -> Option<&str> {
        [
            self.metadata.title.as_str(),
            self.page_title.as_str(),
            self.label.as_str(),
            self.metadata.source_url.as_str(),
            self.url.as_str(),
        ]
        .into_iter()
        .map(str::trim)
        .find(|value| !value.is_empty())
    }

    fn source_url(&self) -> &str {
        if self.metadata.source_url.trim().is_empty() {
            self.url.as_str()
        } else {
            self.metadata.source_url.as_str()
        }
    }
}

fn parse_status_body(body: &str) -> Result<BrowserMprisStatus, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("browser MPRIS status JSON: {err}"))?;
    if value.get("op").and_then(serde_json::Value::as_str) != Some("browser_media_status") {
        return Err("browser MPRIS status has the wrong op".to_owned());
    }
    let state = value
        .get("state")
        .and_then(serde_json::Value::as_str)
        .and_then(PlaybackState::from_wire)
        .ok_or_else(|| "browser MPRIS status has an unsupported state".to_owned())?;
    let metadata = value
        .get("metadata")
        .filter(|value| value.is_object())
        .map(BrowserMprisMetadata::from_value)
        .unwrap_or_default();
    Ok(BrowserMprisStatus {
        state,
        tab_id: value.get("tab_id").and_then(serde_json::Value::as_u64),
        url: status_str(&value, "url", 2048).unwrap_or_default(),
        page_title: status_str(&value, "page_title", 160).unwrap_or_default(),
        label: status_str(&value, "label", 160).unwrap_or_default(),
        metadata,
        updated_ms: value
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

fn status_str(value: &serde_json::Value, key: &str, max_chars: usize) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(max_chars).collect())
}

fn track_path(status: &BrowserMprisStatus) -> String {
    if !status.has_track() {
        return NO_TRACK.to_owned();
    }
    status.tab_id.map_or_else(
        || "/org/mackes/mde/browser/track/active".to_owned(),
        |tab_id| format!("/org/mackes/mde/browser/track/tab_{tab_id}"),
    )
}

fn us_from_ms(ms: u64) -> i64 {
    i64::try_from(ms.saturating_mul(1000)).unwrap_or(i64::MAX)
}

fn insert_str(map: &mut HashMap<String, OwnedValue>, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    if let Ok(value) = OwnedValue::try_from(Value::from(value.to_owned())) {
        map.insert(key.to_owned(), value);
    }
}

fn metadata_map(status: &BrowserMprisStatus) -> HashMap<String, OwnedValue> {
    let mut map = HashMap::new();
    if let Ok(path) = ObjectPath::try_from(track_path(status)) {
        if let Ok(value) = OwnedValue::try_from(Value::from(path)) {
            map.insert("mpris:trackid".to_owned(), value);
        }
    }
    if !status.has_track() {
        return map;
    }
    if let Some(title) = status.title() {
        insert_str(&mut map, "xesam:title", title);
    }
    insert_str(&mut map, "xesam:album", &status.metadata.album);
    insert_str(&mut map, "xesam:url", status.source_url());
    insert_str(&mut map, "mpris:artUrl", &status.metadata.artwork_url);
    if !status.metadata.artist.trim().is_empty() {
        if let Ok(value) = OwnedValue::try_from(Value::from(vec![status.metadata.artist.clone()])) {
            map.insert("xesam:artist".to_owned(), value);
        }
    }
    if status.metadata.duration_ms > 0 {
        if let Ok(value) =
            OwnedValue::try_from(Value::from(us_from_ms(status.metadata.duration_ms)))
        {
            map.insert("mpris:length".to_owned(), value);
        }
    }
    map
}

fn volume(status: &BrowserMprisStatus) -> f64 {
    status
        .metadata
        .volume_percent
        .map(|percent| percent.min(100) as f64 / 100.0)
        .unwrap_or(1.0)
}

fn volume_steps(current_percent: Option<u64>, target_volume: f64) -> Vec<MediaTransportAction> {
    let Some(current_percent) = current_percent else {
        return Vec::new();
    };
    if !target_volume.is_finite() {
        return Vec::new();
    }
    let current = i64::try_from(current_percent.min(100)).unwrap_or(100);
    let target = (target_volume.clamp(0.0, 1.0) * 100.0).round() as i64;
    let delta = target - current;
    if delta == 0 {
        return Vec::new();
    }
    let steps = ((delta.abs() + VOLUME_STEP_PERCENT - 1) / VOLUME_STEP_PERCENT) as usize;
    let action = if delta > 0 {
        MediaTransportAction::VolumeUp
    } else {
        MediaTransportAction::VolumeDown
    };
    std::iter::repeat(action)
        .take(steps.min(MAX_VOLUME_STEPS))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .ok()
        .and_then(|ms| u64::try_from(ms).ok())
        .unwrap_or_default()
}

/// `org.mpris.MediaPlayer2` root interface for Browser.
struct MediaPlayer2;

#[interface(name = "org.mpris.MediaPlayer2")]
impl MediaPlayer2 {
    async fn raise(&self) {}

    async fn quit(&self) {}

    #[zbus(property)]
    async fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn identity(&self) -> String {
        "MDE Browser".to_owned()
    }

    #[zbus(property)]
    async fn desktop_entry(&self) -> String {
        "mde-shell-egui".to_owned()
    }

    #[zbus(property)]
    async fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["http".to_owned(), "https".to_owned(), "file".to_owned()]
    }

    #[zbus(property)]
    async fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `org.mpris.MediaPlayer2.Player` adapter over Browser Bus state/actions.
#[derive(Clone)]
struct Player {
    bus_root: Option<PathBuf>,
    host: String,
}

impl Player {
    fn new(bus_root: Option<PathBuf>, host: String) -> Self {
        Self { bus_root, host }
    }

    fn can_publish(&self) -> bool {
        self.bus_root.is_some()
    }

    fn latest_status(&self) -> BrowserMprisStatus {
        let Some(root) = self.bus_root.as_ref() else {
            return BrowserMprisStatus::default();
        };
        let Ok(persist) = Persist::open(root.clone()) else {
            return BrowserMprisStatus::default();
        };
        let topic = browser_media_status_topic(&self.host);
        persist
            .read_latest(&topic)
            .ok()
            .flatten()
            .and_then(|message| message.body)
            .and_then(|body| parse_status_body(&body).ok())
            .unwrap_or_default()
    }

    fn publish_action_for_status(
        &self,
        status: &BrowserMprisStatus,
        action: MediaTransportAction,
        updated_ms: u64,
    ) {
        let Some(root) = self.bus_root.as_ref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.clone()) else {
            return;
        };
        let topic = browser_media_control_topic(&self.host);
        let body = browser_media_control_body(action, status.tab_id, "mpris", updated_ms);
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }

    fn publish_action(&self, action: MediaTransportAction) {
        let status = self.latest_status();
        self.publish_action_for_status(&status, action, now_ms());
    }

    fn publish_volume_target(&self, target_volume: f64) {
        let status = self.latest_status();
        let steps = volume_steps(status.metadata.volume_percent, target_volume);
        let base_ms = now_ms();
        for (index, action) in steps.into_iter().enumerate() {
            let updated_ms = base_ms.saturating_add(u64::try_from(index).unwrap_or_default());
            self.publish_action_for_status(&status, action, updated_ms);
        }
    }

    async fn notify_transport(&self, emitter: &SignalEmitter<'_>) {
        let _ = self.playback_status_changed(emitter).await;
        let _ = self.metadata_changed(emitter).await;
        let _ = self.position_changed(emitter).await;
    }

    async fn notify_volume(&self, emitter: &SignalEmitter<'_>) {
        let _ = self.volume_changed(emitter).await;
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn play(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.publish_action(MediaTransportAction::Play);
        self.notify_transport(&emitter).await;
    }

    async fn pause(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.publish_action(MediaTransportAction::Pause);
        self.notify_transport(&emitter).await;
    }

    async fn play_pause(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.publish_action(MediaTransportAction::PlayPause);
        self.notify_transport(&emitter).await;
    }

    async fn stop(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.publish_action(MediaTransportAction::Stop);
        self.notify_transport(&emitter).await;
    }

    async fn next(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.publish_action(MediaTransportAction::Next);
        self.notify_transport(&emitter).await;
    }

    async fn previous(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.publish_action(MediaTransportAction::Previous);
        self.notify_transport(&emitter).await;
    }

    async fn seek(&self, _offset: i64) {}

    async fn set_position(&self, _track_id: ObjectPath<'_>, _position: i64) {}

    async fn open_uri(&self, _uri: String) {}

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.latest_status().state.as_mpris().to_owned()
    }

    #[zbus(property)]
    async fn loop_status(&self) -> String {
        "None".to_owned()
    }

    #[zbus(property)]
    async fn set_loop_status(&self, _value: String) {}

    #[zbus(property)]
    async fn shuffle(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn set_shuffle(&self, _value: bool) {}

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        metadata_map(&self.latest_status())
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        volume(&self.latest_status())
    }

    #[zbus(property)]
    async fn set_volume(&self, value: f64, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.publish_volume_target(value);
        self.notify_volume(&emitter).await;
    }

    #[zbus(property)]
    async fn position(&self) -> i64 {
        us_from_ms(self.latest_status().metadata.position_ms)
    }

    #[zbus(property)]
    async fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn can_go_next(&self) -> bool {
        self.can_publish()
    }

    #[zbus(property)]
    async fn can_go_previous(&self) -> bool {
        self.can_publish()
    }

    #[zbus(property)]
    async fn can_play(&self) -> bool {
        self.can_publish()
    }

    #[zbus(property)]
    async fn can_pause(&self) -> bool {
        self.can_publish()
    }

    #[zbus(property)]
    async fn can_seek(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn can_control(&self) -> bool {
        self.can_publish()
    }
}

/// Handle keeping the Browser MPRIS service alive for the shell lifetime.
pub(crate) struct BrowserMprisHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl BrowserMprisHandle {
    /// Signal the service thread to stop and join it.
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for BrowserMprisHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start Browser's FDO MPRIS surface on its own thread.
#[must_use]
pub(crate) fn spawn(bus_root: Option<PathBuf>, host: String) -> BrowserMprisHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("mde-browser-mpris".to_owned())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::warn!(error = %err, "Browser MPRIS runtime unavailable");
                    return;
                }
            };
            rt.block_on(async move {
                let player = Player::new(bus_root, host);
                let built = zbus::connection::Builder::session()
                    .and_then(|builder| builder.name(BUS_NAME))
                    .and_then(|builder| builder.serve_at(OBJECT_PATH, MediaPlayer2))
                    .and_then(|builder| builder.serve_at(OBJECT_PATH, player));
                let _connection = match built {
                    Ok(builder) => match builder.build().await {
                        Ok(connection) => connection,
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "no Browser MPRIS session bus; desktop media bridge disabled"
                            );
                            return;
                        }
                    },
                    Err(err) => {
                        tracing::warn!(error = %err, "Browser MPRIS setup failed");
                        return;
                    }
                };
                while !stop_thread.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        })
        .ok();
    BrowserMprisHandle { stop, join }
}

#[cfg(test)]
mod tests {
    use super::super::wire::parse_browser_media_control_request;
    use super::*;

    fn playing_status_body() -> String {
        serde_json::json!({
            "op": "browser_media_status",
            "source": "browser",
            "node": "alpha",
            "host": "alpha",
            "state": "playing",
            "tab_index": 0,
            "tab_id": 42,
            "engine": "cef",
            "active_tab": false,
            "url": "https://page.example/watch",
            "page_title": "Page Title",
            "label": "Now: Song - Artist",
            "audible": true,
            "muted": false,
            "metadata": {
                "title": "Song",
                "artist": "Artist",
                "album": "Album",
                "artwork_url": "https://media.example/art.png",
                "source_url": "https://media.example/song.mp3",
                "paused": false,
                "duration_ms": 123000,
                "position_ms": 45000,
                "volume_percent": 42
            },
            "updated_ms": 99
        })
        .to_string()
    }

    #[test]
    fn browser_mpris_status_parses_playback_metadata_and_volume() {
        let status = parse_status_body(&playing_status_body()).expect("status");
        assert_eq!(status.state.as_mpris(), "Playing");
        assert_eq!(status.tab_id, Some(42));
        assert_eq!(status.title(), Some("Song"));
        assert_eq!(status.source_url(), "https://media.example/song.mp3");
        assert_eq!(status.metadata.duration_ms, 123000);
        assert_eq!(status.metadata.position_ms, 45000);
        assert_eq!(volume(&status), 0.42);
    }

    #[test]
    fn browser_mpris_metadata_map_uses_valid_track_path_and_optional_fields() {
        let status = parse_status_body(&playing_status_body()).expect("status");
        assert!(ObjectPath::try_from(track_path(&status)).is_ok());
        let metadata = metadata_map(&status);
        assert!(metadata.contains_key("mpris:trackid"));
        assert!(metadata.contains_key("xesam:title"));
        assert!(metadata.contains_key("xesam:artist"));
        assert!(metadata.contains_key("xesam:album"));
        assert!(metadata.contains_key("mpris:artUrl"));
        assert!(metadata.contains_key("mpris:length"));

        let idle = BrowserMprisStatus::default();
        assert_eq!(track_path(&idle), NO_TRACK);
        let metadata = metadata_map(&idle);
        assert!(metadata.contains_key("mpris:trackid"));
        assert!(!metadata.contains_key("xesam:title"));
    }

    #[test]
    fn browser_mpris_volume_set_maps_to_bounded_browser_step_actions() {
        assert_eq!(
            volume_steps(Some(42), 0.57),
            vec![
                MediaTransportAction::VolumeUp,
                MediaTransportAction::VolumeUp,
                MediaTransportAction::VolumeUp
            ]
        );
        assert_eq!(
            volume_steps(Some(42), 0.25),
            vec![
                MediaTransportAction::VolumeDown,
                MediaTransportAction::VolumeDown,
                MediaTransportAction::VolumeDown,
                MediaTransportAction::VolumeDown
            ]
        );
        assert!(volume_steps(None, 0.25).is_empty());
        assert_eq!(volume_steps(Some(100), 0.0).len(), MAX_VOLUME_STEPS);
    }

    #[test]
    fn browser_mpris_transport_publishes_existing_browser_control_body() {
        let bus = tempfile::tempdir().expect("temp bus");
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        persist
            .write(
                &browser_media_status_topic("alpha"),
                Priority::Default,
                None,
                Some(&playing_status_body()),
            )
            .expect("write status");

        let player = Player::new(Some(bus.path().to_path_buf()), "alpha".to_owned());
        player.publish_action(MediaTransportAction::Pause);

        let latest = persist
            .read_latest(&browser_media_control_topic("alpha"))
            .expect("read media control")
            .expect("control message");
        let body = latest.body.expect("control body");
        let request = parse_browser_media_control_request(&body).expect("parse media control");
        assert_eq!(request.action, MediaTransportAction::Pause);
        assert_eq!(request.tab_id, Some(42));
        let json: serde_json::Value = serde_json::from_str(&body).expect("control json");
        assert_eq!(json["source"], "mpris");
    }

    #[test]
    fn browser_mpris_service_owns_fdo_name_when_session_bus_exists() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return;
        }
        let bus = tempfile::tempdir().expect("temp bus");
        let mut handle = spawn(Some(bus.path().to_path_buf()), "alpha".to_owned());
        let connection = zbus::blocking::Connection::session().expect("session bus");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let reply = connection
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &BUS_NAME,
                )
                .expect("NameHasOwner call");
            let owned: bool = reply.body().deserialize().expect("NameHasOwner bool");
            if owned {
                handle.stop();
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        handle.stop();
        panic!("{BUS_NAME} was not owned on the session bus");
    }
}

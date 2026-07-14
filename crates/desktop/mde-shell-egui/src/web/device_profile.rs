//! Per-tab browser identity/appearance profiles — the small closed enums that a
//! tab carries and mirrors to the helper: which isolation container it runs in,
//! which display it should land on, the page-visible User-Agent / device-profile
//! overrides, and the device-permission kinds plus a per-site permission-prompt
//! record — plus the site-permission [`WebState`] actions that read and mutate that
//! record (summarize the active site's prompts, forget them, and deny a device
//! prompt to the bus). `use super::*` pulls in the parent's `host_of` /
//! `browser_permission_prompt_body` / `publish_to_bus` helpers and `WebState`'s
//! private fields. A pure relocation from the `web` god-module.

use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ContainerProfile {
    #[default]
    None,
    Personal,
    Work,
    Banking,
    Research,
}

impl ContainerProfile {
    pub(super) const ALL: [Self; 5] = [
        Self::None,
        Self::Personal,
        Self::Work,
        Self::Banking,
        Self::Research,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::None => "No Container",
            Self::Personal => "Personal",
            Self::Work => "Work",
            Self::Banking => "Banking",
            Self::Research => "Research",
        }
    }

    pub(super) const fn chip(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Personal => "Personal",
            Self::Work => "Work",
            Self::Banking => "Banking",
            Self::Research => "Research",
        }
    }

    pub(super) const fn marker(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Personal => "P ",
            Self::Work => "W ",
            Self::Banking => "B ",
            Self::Research => "R ",
        }
    }

    pub(super) const fn wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Personal => "personal",
            Self::Work => "work",
            Self::Banking => "banking",
            Self::Research => "research",
        }
    }

    pub(super) const fn next(self) -> Self {
        match self {
            Self::None => Self::Personal,
            Self::Personal => Self::Work,
            Self::Work => Self::Banking,
            Self::Banking => Self::Research,
            Self::Research => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum DisplayTarget {
    #[default]
    Current,
    Primary,
    Secondary,
    AllDisplays,
}

impl DisplayTarget {
    pub(super) const ALL: [Self; 4] = [
        Self::Current,
        Self::Primary,
        Self::Secondary,
        Self::AllDisplays,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Current => "Current Display",
            Self::Primary => "Primary Display",
            Self::Secondary => "Secondary Display",
            Self::AllDisplays => "All Displays",
        }
    }

    pub(super) const fn chip(self) -> &'static str {
        match self {
            Self::Current => "",
            Self::Primary => "Display 1",
            Self::Secondary => "Display 2",
            Self::AllDisplays => "All Displays",
        }
    }

    pub(super) const fn marker(self) -> &'static str {
        match self {
            Self::Current => "",
            Self::Primary => "D1 ",
            Self::Secondary => "D2 ",
            Self::AllDisplays => "DA ",
        }
    }

    pub(super) const fn wire(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::AllDisplays => "all_displays",
        }
    }

    pub(super) const fn next(self) -> Self {
        match self {
            Self::Current => Self::Primary,
            Self::Primary => Self::Secondary,
            Self::Secondary => Self::AllDisplays,
            Self::AllDisplays => Self::Current,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DevicePermissionKind {
    Camera,
    Microphone,
    Location,
    Notifications,
    Clipboard,
}

impl DevicePermissionKind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Camera => "Camera",
            Self::Microphone => "Microphone",
            Self::Location => "Location",
            Self::Notifications => "Notifications",
            Self::Clipboard => "Clipboard",
        }
    }

    pub(super) const fn wire(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::Microphone => "microphone",
            Self::Location => "location",
            Self::Notifications => "notifications",
            Self::Clipboard => "clipboard",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SitePermissionPrompt {
    pub(super) host: String,
    pub(super) kind: DevicePermissionKind,
    pub(super) decision: &'static str,
    pub(super) updated_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum UserAgentOverride {
    #[default]
    Default,
    DesktopChrome,
    AndroidChrome,
}

impl UserAgentOverride {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Default => "Default User Agent",
            Self::DesktopChrome => "Desktop Chrome",
            Self::AndroidChrome => "Android Chrome",
        }
    }

    pub(super) const fn chip(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::DesktopChrome => "UA Desktop",
            Self::AndroidChrome => "UA Mobile",
        }
    }

    pub(super) const fn marker(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::DesktopChrome => "UA ",
            Self::AndroidChrome => "UAm ",
        }
    }

    pub(super) const fn wire(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::DesktopChrome => "desktop_chrome",
            Self::AndroidChrome => "android_chrome",
        }
    }

    pub(super) const fn value(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::DesktopChrome => {
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
            }
            Self::AndroidChrome => {
                "Mozilla/5.0 (Linux; Android 14; MDE Mesh) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36"
            }
        }
    }

    pub(super) const fn next(self) -> Self {
        match self {
            Self::Default => Self::DesktopChrome,
            Self::DesktopChrome => Self::AndroidChrome,
            Self::AndroidChrome => Self::Default,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum DeviceProfile {
    #[default]
    Default,
    Phone,
    Tablet,
}

impl DeviceProfile {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Default => "Default Device",
            Self::Phone => "Phone",
            Self::Tablet => "Tablet",
        }
    }

    pub(super) const fn chip(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Phone => "Device Phone",
            Self::Tablet => "Device Tablet",
        }
    }

    pub(super) const fn marker(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Phone => "Ph ",
            Self::Tablet => "Tb ",
        }
    }

    pub(super) const fn wire(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
        }
    }

    pub(super) const fn dimensions(self) -> (u16, u16, u16, bool) {
        match self {
            Self::Default => (0, 0, 100, false),
            Self::Phone => (390, 844, 300, true),
            Self::Tablet => (820, 1180, 200, true),
        }
    }

    pub(super) const fn next(self) -> Self {
        match self {
            Self::Default => Self::Phone,
            Self::Phone => Self::Tablet,
            Self::Tablet => Self::Default,
        }
    }
}

/// Site-permission actions on the Browser surface state — summarizing the active
/// first-party site's sensitive-prompt decisions, forgetting them, and denying a
/// device-permission prompt (mirrored to the daemon over the bus) — kept beside the
/// [`SitePermissionPrompt`] / [`DevicePermissionKind`] value types they operate on.
/// A pure relocation from the `web` god-module.
impl WebState {
    pub(super) fn active_site_permission_summary(&self) -> Option<String> {
        let host = self.active_first_party()?;
        if self
            .forgotten_permission_sites
            .iter()
            .any(|site| site == &host)
        {
            return Some(format!("{host}: forgotten; default deny remains active"));
        }
        let prompts = self
            .site_permission_prompts
            .iter()
            .filter(|prompt| prompt.host == host)
            .map(|prompt| format!("{} {}", prompt.kind.wire(), prompt.decision))
            .collect::<Vec<_>>();
        if prompts.is_empty() {
            Some(format!("{host}: all sensitive prompts denied by default"))
        } else {
            Some(format!(
                "{host}: {}; helper default deny remains active",
                prompts.join(", ")
            ))
        }
    }

    pub(super) fn forget_active_site_permissions(&mut self) {
        let Some((host, engine, url, title)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            host_of(&url).map(|host| (host, tab.engine, url, tab.session.title().to_owned()))
        }) else {
            return;
        };
        let revoked_grants = self
            .granted_permissions
            .iter()
            .filter(|(origin, _)| host_of(origin).as_deref() == Some(host.as_str()))
            .count();
        let cleared_prompt_decisions = self
            .site_permission_prompts
            .iter()
            .filter(|prompt| prompt.host == host)
            .count();
        let now = unix_ms();
        self.granted_permissions
            .retain(|(origin, _)| host_of(origin).as_deref() != Some(host.as_str()));
        self.forgotten_permission_sites.retain(|site| site != &host);
        self.site_permission_prompts
            .retain(|prompt| prompt.host != host);
        self.forgotten_permission_sites.push(host.clone());
        let body = browser_permission_revoke_body(
            engine,
            &url,
            &title,
            &host,
            revoked_grants,
            cleared_prompt_decisions,
            now,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_PERMISSION_REVOKE,
            &body,
        );
    }

    pub(super) fn prompt_active_device_permission(&mut self, kind: DevicePermissionKind) {
        let Some((url, title, engine, host)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            if url.is_empty() || tab.session.is_crashed() {
                None
            } else {
                host_of(&url).map(|host| (url, tab.session.title().to_owned(), tab.engine, host))
            }
        }) else {
            self.capture_notice = Some(format!("{} prompt requires a live site", kind.label()));
            return;
        };
        let now = unix_ms();
        if let Some(prompt) = self
            .site_permission_prompts
            .iter_mut()
            .find(|prompt| prompt.host == host && prompt.kind == kind)
        {
            prompt.decision = "denied";
            prompt.updated_ms = now;
        } else {
            self.site_permission_prompts.push(SitePermissionPrompt {
                host: host.clone(),
                kind,
                decision: "denied",
                updated_ms: now,
            });
        }
        self.forgotten_permission_sites.retain(|site| site != &host);
        let body = browser_permission_prompt_body(kind, engine, &url, &title, &host, now);
        publish_to_bus(
            self.bus_root.as_deref(),
            ACTION_BROWSER_PERMISSION_PROMPT,
            &body,
        );
        self.capture_notice = Some(format!(
            "{} prompt denied for {host}; helper default deny remains active",
            kind.label()
        ));
    }
}

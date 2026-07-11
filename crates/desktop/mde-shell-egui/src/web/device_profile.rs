//! Per-tab browser identity/appearance profiles — the small closed enums that a
//! tab carries and mirrors to the helper: which isolation container it runs in,
//! which display it should land on, the page-visible User-Agent / device-profile
//! overrides, and the device-permission kinds plus a per-site permission-prompt
//! record. Self-contained value types (no `WebState` coupling); `use super::*`
//! only pulls in std for the derives. A pure relocation from the `web` god-module.

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

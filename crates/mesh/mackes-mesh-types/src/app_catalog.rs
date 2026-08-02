//! Versioned, fail-closed catalog records for guest-owned Flatpak apps.
//!
//! The catalog is data, not a launcher: no field in this contract is an
//! executable, mount point, environment, or host socket. Consumers must
//! validate the catalog before projecting it into Front Door or creating an
//! [`crate::vdi_session::AppVmLaunchRequest`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The only catalog schema currently admitted by the App VM path.
pub const FLATPAK_CATALOG_SCHEMA_VERSION: u16 = 1;
const MAX_ID_BYTES: usize = 255;
const MAX_TEXT_BYTES: usize = 1024;

/// A signed, versioned set of curated guest applications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatpakAppCatalog {
    /// Schema discriminator for deterministic consumer behavior.
    pub schema_version: u16,
    /// Monotonic catalog revision selected by the signed provider.
    pub revision: String,
    /// Catalog rows, with unique app IDs after validation.
    pub entries: Vec<FlatpakCatalogEntry>,
}

/// One catalog row. The row contains only an identity and approved policy
/// metadata; guest provisioning resolves the profile through its own allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatpakCatalogEntry {
    /// Stable reverse-DNS Flatpak identity.
    pub app_id: String,
    /// User-facing application name.
    pub display_name: String,
    /// Bounded user-facing summary.
    pub summary: String,
    /// Non-executable icon reference resolved by the guest/UI catalog.
    pub icon_reference: String,
    /// Approved source revision for this app.
    pub source_revision: String,
    /// Capabilities admitted by the guest profile policy.
    pub declared_capabilities: Vec<String>,
    /// Named guest profile, never an image path or command.
    pub guest_profile: String,
    /// Actions exposed by the curated guest declaration.
    pub supported_actions: Vec<String>,
    /// Source and signature provenance.
    pub provenance: FlatpakCatalogProvenance,
    /// Explicit install/readiness state.
    pub state: FlatpakInstallState,
}

/// Provenance needed before a catalog row can become launchable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatpakCatalogProvenance {
    /// Curated provider or repository identity.
    pub source: String,
    /// Detached signature or equivalent signed-evidence reference.
    pub signature: Option<String>,
}

/// Installation/readiness is explicit so missing or stale content is never a
/// launchable-looking Front Door result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlatpakInstallState {
    /// Guest content is installed and may be launchable if signed.
    Installed,
    /// Catalog metadata exists but guest content is not installed.
    Available,
    /// Installed content no longer matches the admitted catalog revision.
    Stale,
    /// The row lacks trusted provenance.
    Unsigned,
    /// The guest provider cannot currently supply the app.
    Unavailable,
}

/// Why catalog validation rejected an untrusted record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatpakCatalogError {
    /// The consumer does not implement this schema version.
    UnsupportedSchema(u16),
    /// A bounded identity or text field is blank or contains controls.
    InvalidField(&'static str),
    /// A bounded field exceeds its wire limit.
    FieldTooLong(&'static str),
    /// The app ID is not a reverse-DNS identity.
    InvalidAppId,
    /// A capability/action list contains an unsafe or repeated value.
    InvalidListValue(&'static str),
    /// Two catalog rows claim the same stable app ID.
    DuplicateAppId,
    /// The selected App VM profile does not implement this capability safely.
    UnsupportedCapability(String),
}

impl FlatpakAppCatalog {
    /// Validate the complete catalog before it crosses a provider boundary.
    pub fn validate(&self) -> Result<(), FlatpakCatalogError> {
        if self.schema_version != FLATPAK_CATALOG_SCHEMA_VERSION {
            return Err(FlatpakCatalogError::UnsupportedSchema(self.schema_version));
        }
        validate_text("revision", &self.revision, MAX_ID_BYTES)?;
        let mut app_ids = BTreeSet::new();
        for entry in &self.entries {
            entry.validate()?;
            if !app_ids.insert(&entry.app_id) {
                return Err(FlatpakCatalogError::DuplicateAppId);
            }
        }
        Ok(())
    }

    /// Return a deterministic, validated catalog from untrusted input.
    pub fn admitted(self) -> Result<Self, FlatpakCatalogError> {
        self.validate()?;
        Ok(self)
    }
}

impl FlatpakCatalogEntry {
    fn validate(&self) -> Result<(), FlatpakCatalogError> {
        if !is_flatpak_app_id(&self.app_id) {
            return Err(FlatpakCatalogError::InvalidAppId);
        }
        validate_text("display_name", &self.display_name, MAX_TEXT_BYTES)?;
        validate_text("summary", &self.summary, MAX_TEXT_BYTES)?;
        validate_text("icon_reference", &self.icon_reference, MAX_ID_BYTES)?;
        validate_text("source_revision", &self.source_revision, MAX_ID_BYTES)?;
        validate_text("guest_profile", &self.guest_profile, MAX_ID_BYTES)?;
        validate_list(&self.declared_capabilities, "declared_capabilities")?;
        for capability in &self.declared_capabilities {
            if !crate::cloud::APP_VM_ALLOWED_CAPABILITIES.contains(&capability.as_str()) {
                return Err(FlatpakCatalogError::UnsupportedCapability(
                    capability.clone(),
                ));
            }
        }
        validate_list(&self.supported_actions, "supported_actions")?;
        validate_text("provenance.source", &self.provenance.source, MAX_ID_BYTES)?;
        if let Some(signature) = &self.provenance.signature {
            validate_text("provenance.signature", signature, MAX_TEXT_BYTES)?;
        }
        Ok(())
    }

    /// Only installed, signed rows can be handed to the launch/session layer.
    #[must_use]
    pub fn is_launchable(&self) -> bool {
        self.validate().is_ok()
            && self.state == FlatpakInstallState::Installed
            && self
                .provenance
                .signature
                .as_deref()
                .is_some_and(|signature| !signature.trim().is_empty())
    }
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), FlatpakCatalogError> {
    if value.trim().is_empty()
        || value.chars().any(char::is_control)
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(FlatpakCatalogError::InvalidField(field));
    }
    if value.len() > max_bytes {
        return Err(FlatpakCatalogError::FieldTooLong(field));
    }
    Ok(())
}

fn validate_list(values: &[String], field: &'static str) -> Result<(), FlatpakCatalogError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if value.trim().is_empty()
            || value
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
            || value.contains('/')
            || value.contains('\\')
            || value.len() > MAX_ID_BYTES
            || !seen.insert(value)
        {
            return Err(FlatpakCatalogError::InvalidListValue(field));
        }
    }
    Ok(())
}

fn is_flatpak_app_id(value: &str) -> bool {
    if value.len() > MAX_ID_BYTES || value.trim() != value {
        return false;
    }
    let mut components = value.split('.');
    let mut count = 0;
    components.all(|component| {
        count += 1;
        !component.is_empty()
            && component.len() <= 63
            && component.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '_' || character == '-'
            })
            && component
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
    }) && count >= 2
}

/// Validate one reverse-DNS Flatpak identity at a launch boundary without
/// requiring a complete catalog row.
#[must_use]
pub fn is_valid_flatpak_app_id(value: &str) -> bool {
    is_flatpak_app_id(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(app_id: &str) -> FlatpakCatalogEntry {
        FlatpakCatalogEntry {
            app_id: app_id.into(),
            display_name: "Editor".into(),
            summary: "A guest-owned editor".into(),
            icon_reference: "icon:editor".into(),
            source_revision: "flathub:42".into(),
            declared_capabilities: vec!["audio".into(), "clipboard".into()],
            guest_profile: "wayland-standard".into(),
            supported_actions: vec!["launch".into(), "resume".into()],
            provenance: FlatpakCatalogProvenance {
                source: "curated".into(),
                signature: Some("sig-42".into()),
            },
            state: FlatpakInstallState::Installed,
        }
    }

    #[test]
    fn catalog_admits_unique_signed_installed_rows() {
        let catalog = FlatpakAppCatalog {
            schema_version: FLATPAK_CATALOG_SCHEMA_VERSION,
            revision: "catalog-42".into(),
            entries: vec![entry("org.example.Editor"), entry("org.example.Terminal")],
        };
        assert!(catalog.clone().admitted().is_ok());
        assert!(catalog.entries[0].is_launchable());
        let body = serde_json::to_string(&catalog).expect("catalog JSON");
        assert!(body.contains("guest_profile"));
        assert!(!body.contains("command"));
    }

    #[test]
    fn catalog_rejects_duplicate_or_malformed_rows() {
        let duplicate = FlatpakAppCatalog {
            schema_version: FLATPAK_CATALOG_SCHEMA_VERSION,
            revision: "catalog-42".into(),
            entries: vec![entry("org.example.Editor"), entry("org.example.Editor")],
        };
        assert_eq!(
            duplicate.admitted(),
            Err(FlatpakCatalogError::DuplicateAppId)
        );

        let mut malformed = entry("org.example.Editor");
        malformed.guest_profile = "/tmp/image".into();
        assert_eq!(
            FlatpakAppCatalog {
                schema_version: FLATPAK_CATALOG_SCHEMA_VERSION,
                revision: "catalog-42".into(),
                entries: vec![malformed],
            }
            .admitted(),
            Err(FlatpakCatalogError::InvalidField("guest_profile"))
        );
    }

    #[test]
    fn catalog_rejects_capabilities_the_app_vm_profile_cannot_serve() {
        let mut unsupported = entry("org.example.Editor");
        unsupported.declared_capabilities = vec!["gpu".into()];
        assert_eq!(
            FlatpakAppCatalog {
                schema_version: FLATPAK_CATALOG_SCHEMA_VERSION,
                revision: "catalog-42".into(),
                entries: vec![unsupported],
            }
            .admitted(),
            Err(FlatpakCatalogError::UnsupportedCapability("gpu".into()))
        );
    }

    #[test]
    fn unsigned_or_uninstalled_rows_are_not_launchable() {
        let mut row = entry("org.example.Editor");
        row.provenance.signature = None;
        assert!(!row.is_launchable());
        row.provenance.signature = Some("sig-42".into());
        row.state = FlatpakInstallState::Stale;
        assert!(!row.is_launchable());
    }

    #[test]
    fn malformed_installed_rows_are_not_launchable_before_catalog_projection() {
        let mut row = entry("org.example.Editor");
        row.guest_profile = "/tmp/image".into();
        assert!(!row.is_launchable());

        row.guest_profile = "wayland-standard".into();
        row.declared_capabilities = vec!["gpu".into()];
        assert!(!row.is_launchable());
    }
}

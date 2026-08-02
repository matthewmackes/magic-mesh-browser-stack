//! Provider-neutral Construct Cloud shared contracts — the SOLE definition site
//! for the mesh cloud's §6 wire shapes.
//!
//! This module owns the pure service-directory / API-health / resource-table /
//! stack (IaC) types the mesh-side producer publishes and the desktop-side
//! Infra-as-Code + Cloud surfaces consume. Neither crate may depend on the other
//! (the layered-tiers boundary gate, §6), so the shapes live here in the
//! mesh-neutral shared crate — alongside [`crate::device_inventory`] — and both
//! sides `use mackes_mesh_types::cloud::*`.
//!
//! ## What lives here (pure, no I/O)
//!
//! - [`ServiceCatalog`] — the authoritative service directory the surface renders.
//!   [`ServiceCatalog::from_keystone_token_json`] parses the mirror's catalog JSON
//!   (a provider-neutral parser that accepts the standard `token.catalog[]` shape)
//!   into it.
//! - [`ServiceHealth`] — a per-endpoint API health row (`state`/`latency_ms`/
//!   `microversion`/`version_id`). [`shape_health`] turns a raw [`ProbeOutcome`]
//!   into it honestly — an unreachable endpoint reads [`HealthState::Down`], an
//!   absent one [`HealthState::Absent`], never a fabricated `up` (§7).
//! - [`ResourceTable`] / [`HeatStackDetail`] / [`HeatPreview`] — the read-only
//!   resource tables + the stack (IaC) detail/preview shapes.
//! - [`CloudInstance`] / [`LifecycleAction`] / [`CloudReply`] +
//!   [`cloud_action_topic`] — the neutral `action/cloud/*` lifecycle command
//!   contract the fleet command surface rides.
//!
//! The I/O (minting the mirror, issuing probes, converging the backend) belongs to
//! the mesh-side worker; only these pure types + parsers are shared, so the
//! backend can be swapped without the consumer knowing.
use serde::{Deserialize, Serialize};

// ─────────────────────────── the service catalog ───────────────────────────

/// A Keystone endpoint interface (`public` / `internal` / `admin`) — the three
/// URLs a service advertises for one region.
///
/// The mesh cloud reaches every API over its Nebula overlay, so in practice the
/// three interfaces resolve to the same overlay URL; the distinction is
/// preserved because it is part of the standard catalog the surface renders and
/// a real deployment may split them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointInterface {
    /// The tenant-facing URL (what a mesh client uses).
    Public,
    /// The service-to-service URL.
    Internal,
    /// The admin URL.
    Admin,
}

impl EndpointInterface {
    /// Every interface, in the canonical (catalog) order.
    pub const ALL: [Self; 3] = [Self::Public, Self::Internal, Self::Admin];

    /// The lowercase catalog token (`"public"` / `"internal"` / `"admin"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Admin => "admin",
        }
    }

    /// Parse a Keystone interface token, tolerating case + surrounding space.
    /// `None` for an unrecognized interface (never guessed).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "public" => Some(Self::Public),
            "internal" => Some(Self::Internal),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// One advertised endpoint of a cataloged service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEndpoint {
    /// Which interface this URL serves.
    pub interface: EndpointInterface,
    /// The endpoint URL (may carry a version suffix, e.g. `.../v2.1`).
    pub url: String,
    /// The region this endpoint lives in (the catalog's `region`/`region_id`),
    /// when advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// One service in the Keystone catalog — its type, its human name, and its
/// per-interface endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogService {
    /// The service **type** — the stable key the surface groups + drills on
    /// (`compute` / `network` / `image` / `volumev3` / `orchestration` /
    /// `identity` / …).
    #[serde(rename = "type")]
    pub service_type: String,
    /// The service's human name when the catalog carries one (`nova`, `neutron`,
    /// `glance`, …). `None` when the deployment left it unset (honest, not
    /// guessed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The advertised endpoints, in catalog order.
    pub endpoints: Vec<CatalogEndpoint>,
}

impl CatalogService {
    /// The endpoint serving `interface`, if this service advertises one.
    #[must_use]
    pub fn endpoint(&self, interface: EndpointInterface) -> Option<&CatalogEndpoint> {
        self.endpoints.iter().find(|e| e.interface == interface)
    }

    /// The public endpoint URL — what a mesh client connects to. Falls back to
    /// the internal then admin URL when a deployment advertises only those, so
    /// the surface always has a URL to show when *any* endpoint exists.
    #[must_use]
    pub fn primary_url(&self) -> Option<&str> {
        EndpointInterface::ALL
            .iter()
            .find_map(|i| self.endpoint(*i))
            .map(|e| e.url.as_str())
    }
}

/// The whole Keystone service catalog — the authoritative directory the IAC
/// surface consumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceCatalog {
    /// The cloud's region, when the catalog advertises a single one (the design
    /// is a single default context/region, Q19). `None` when the catalog is
    /// empty or spans several regions (the per-endpoint [`CatalogEndpoint::region`]
    /// stays authoritative in that case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Every advertised service, in catalog order.
    pub services: Vec<CatalogService>,
}

/// A Keystone catalog couldn't be parsed from a token response — the typed,
/// honest failure (never a fabricated empty catalog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogParseError(pub String);

impl std::fmt::Display for CatalogParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parsing the Keystone token catalog failed: {}", self.0)
    }
}

impl std::error::Error for CatalogParseError {}

impl ServiceCatalog {
    /// Parse a Keystone **v3 token response** body into the service directory.
    ///
    /// The body is the JSON `POST /v3/auth/tokens` returns — `{ "token": {
    /// "catalog": [ { "type": …, "name": …, "endpoints": [ { "interface": …,
    /// "url": …, "region": … } ] } ] } }`. An endpoint whose `interface` isn't
    /// one of public/internal/admin is skipped (never mis-mapped); a service
    /// with no usable endpoint is still listed (its presence is real, §7).
    ///
    /// # Errors
    /// [`CatalogParseError`] when the body isn't valid JSON or has no
    /// `token.catalog` array.
    pub fn from_keystone_token_json(body: &str) -> Result<Self, CatalogParseError> {
        #[derive(Deserialize)]
        struct Root {
            token: Token,
        }
        #[derive(Deserialize)]
        struct Token {
            #[serde(default)]
            catalog: Option<Vec<RawService>>,
        }
        #[derive(Deserialize)]
        struct RawService {
            #[serde(rename = "type")]
            service_type: String,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            endpoints: Vec<RawEndpoint>,
        }
        #[derive(Deserialize)]
        struct RawEndpoint {
            #[serde(default)]
            interface: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            region: Option<String>,
            #[serde(default)]
            region_id: Option<String>,
        }

        let root: Root =
            serde_json::from_str(body.trim()).map_err(|e| CatalogParseError(e.to_string()))?;
        let raw = root
            .token
            .catalog
            .ok_or_else(|| CatalogParseError("token.catalog is absent".to_string()))?;

        let services: Vec<CatalogService> = raw
            .into_iter()
            .map(|s| {
                let endpoints = s
                    .endpoints
                    .into_iter()
                    .filter_map(|e| {
                        let interface = EndpointInterface::parse(&e.interface)?;
                        if e.url.trim().is_empty() {
                            return None;
                        }
                        Some(CatalogEndpoint {
                            interface,
                            url: e.url,
                            region: e.region.or(e.region_id).filter(|r| !r.trim().is_empty()),
                        })
                    })
                    .collect();
                CatalogService {
                    service_type: s.service_type,
                    name: s.name.filter(|n| !n.trim().is_empty()),
                    endpoints,
                }
            })
            .collect();

        // Derive a catalog-wide region only when every endpoint agrees on one
        // (the single-context design); otherwise leave it None and let the
        // per-endpoint region stay authoritative.
        let all_regions: Vec<&str> = services
            .iter()
            .flat_map(|s| s.endpoints.iter())
            .filter_map(|e| e.region.as_deref())
            .collect();
        let region = match all_regions.first() {
            Some(first) if all_regions.iter().all(|r| r == first) => Some((*first).to_string()),
            _ => None,
        };

        Ok(Self { region, services })
    }

    /// The cataloged service of type `service_type`, if advertised.
    #[must_use]
    pub fn service(&self, service_type: &str) -> Option<&CatalogService> {
        self.services
            .iter()
            .find(|s| s.service_type == service_type)
    }
}

// ─────────────────────────── per-service API health ───────────────────────────

/// The honest health of one cataloged endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// The endpoint answered a version/ping probe (any HTTP status counts as
    /// reachable — a `401`/`300` still proves the service is up).
    Up,
    /// The endpoint is cataloged but did not answer (connection refused, a
    /// timeout, or a `5xx`) — the service is down, not faked up.
    Down,
    /// The service advertises no endpoint for the probed interface — there is
    /// nothing to reach (honestly absent, distinct from `down`).
    Absent,
}

/// One per-endpoint health row the IAC status band renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceHealth {
    /// The service type this health is for (`compute`, `network`, …).
    pub service_type: String,
    /// The interface probed.
    pub interface: EndpointInterface,
    /// The endpoint URL probed. Empty when [`HealthState::Absent`] (no endpoint).
    pub url: String,
    /// The honest state.
    pub state: HealthState,
    /// Round-trip latency of the probe in milliseconds. `Some` for a real probe
    /// (`Up` or `Down` after a transport attempt); `None` for `Absent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// The service's advertised microversion (the version doc's `max_version`,
    /// falling back to `version`), when the version document carried one. `None`
    /// for a service that doesn't speak microversions or an unreadable body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microversion: Option<String>,
    /// The version-document id (`v2.1`, `v3`, …), when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// A short human reason (the HTTP status, or the transport error) — the
    /// operator's "why" for a `Down`, and context for an `Up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The raw result of one health probe — the transport layer's honest report.
///
/// [`shape_health`] turns it into a [`ServiceHealth`]. Kept transport-neutral (an
/// HTTP status + body, or a failure) so the producer's client and the tests share
/// one shaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The endpoint answered: the HTTP status, the response body (a version
    /// discovery document when the service exposes one), and the round-trip.
    Reachable {
        /// The HTTP status code the endpoint returned.
        http_status: u16,
        /// The response body (parsed for a microversion when it is a version
        /// document; may be empty).
        body: String,
        /// Round-trip in milliseconds.
        elapsed_ms: u64,
    },
    /// The endpoint could not be reached (connection refused / timeout / DNS) —
    /// the elapsed time until the failure and the transport reason.
    Unreachable {
        /// Time spent before the transport gave up, in milliseconds.
        elapsed_ms: u64,
        /// The transport error (connection refused, timed out, …).
        reason: String,
    },
}

/// Shape a raw [`ProbeOutcome`] for `(service_type, interface, url)` into an
/// honest [`ServiceHealth`].
///
/// A **reachable** outcome reads [`HealthState::Up`] with the latency and, when
/// the body is a version document, its microversion + version id — except a
/// `5xx`, which reads **down** (cataloged but erroring); any other status (incl.
/// `2xx`/`3xx`/`401`) is up (it answered). An **unreachable** outcome reads
/// [`HealthState::Down`] with the latency-to-failure and the transport reason.
/// Never a fabricated `up` (§7).
#[must_use]
pub fn shape_health(
    service_type: &str,
    interface: EndpointInterface,
    url: &str,
    outcome: &ProbeOutcome,
) -> ServiceHealth {
    match outcome {
        ProbeOutcome::Reachable {
            http_status,
            body,
            elapsed_ms,
        } => {
            let (version_id, microversion) = parse_version_document(body);
            let is_server_error = (500..600).contains(http_status);
            ServiceHealth {
                service_type: service_type.to_string(),
                interface,
                url: url.to_string(),
                state: if is_server_error {
                    HealthState::Down
                } else {
                    HealthState::Up
                },
                latency_ms: Some(*elapsed_ms),
                microversion,
                version_id,
                detail: Some(format!("HTTP {http_status}")),
            }
        }
        ProbeOutcome::Unreachable { elapsed_ms, reason } => ServiceHealth {
            service_type: service_type.to_string(),
            interface,
            url: url.to_string(),
            state: HealthState::Down,
            latency_ms: Some(*elapsed_ms),
            microversion: None,
            version_id: None,
            detail: Some(reason.clone()),
        },
    }
}

/// The honest "no endpoint to probe" health row for a service that advertises
/// nothing on `interface`.
#[must_use]
pub fn absent_health(service_type: &str, interface: EndpointInterface) -> ServiceHealth {
    ServiceHealth {
        service_type: service_type.to_string(),
        interface,
        url: String::new(),
        state: HealthState::Absent,
        latency_ms: None,
        microversion: None,
        version_id: None,
        detail: Some("no endpoint advertised for this interface".to_string()),
    }
}

/// Extract `(version_id, microversion)` from an `OpenStack` version-discovery
/// document body, tolerating the three real shapes services emit:
/// `{"versions":[…]}` (Nova/Cinder/Glance root), `{"versions":{"values":[…]}}`
/// (Keystone root), and `{"version":{…}}` (a versioned URL). The microversion is
/// the entry's `max_version` (falling back to a non-empty `version`); the id is
/// its `id`. Returns `(None, None)` when the body isn't a recognizable version
/// document — never a guessed version (§7).
fn parse_version_document(body: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim()) else {
        return (None, None);
    };
    // Collect the candidate version entries from whichever shape is present.
    let entries: Vec<&serde_json::Value> = if let Some(v) = value.get("version") {
        vec![v]
    } else if let Some(list) = value.get("versions").and_then(|v| v.as_array()) {
        list.iter().collect()
    } else if let Some(list) = value
        .get("versions")
        .and_then(|v| v.get("values"))
        .and_then(|v| v.as_array())
    {
        list.iter().collect()
    } else {
        return (None, None);
    };
    if entries.is_empty() {
        return (None, None);
    }
    // Prefer the CURRENT/stable entry; else the last (highest) advertised.
    let chosen = entries
        .iter()
        .find(|e| {
            e.get("status")
                .and_then(|s| s.as_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("current"))
        })
        .or_else(|| entries.last())
        .copied();
    let Some(entry) = chosen else {
        return (None, None);
    };
    let non_empty = |v: &serde_json::Value, key: &str| -> Option<String> {
        v.get(key)
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let version_id = non_empty(entry, "id");
    let microversion = non_empty(entry, "max_version").or_else(|| non_empty(entry, "version"));
    (version_id, microversion)
}

// ─────────────────────────── resource tables (IAC-3) ───────────────────────────

/// The maximum number of value columns a resource table renders.
///
/// Enough to carry a resource's identifying + status fields without an unreadably
/// wide table (the `id` rides [`ResourceRow::id`], not a column).
pub const MAX_RESOURCE_COLUMNS: usize = 7;

/// The default REST **collection path** the IAC Resources tab lists for a
/// Keystone service **type**.
///
/// Appended to the service's catalog endpoint base (IAC-3 / design #10/#14).
/// `None` for a service the tab doesn't drill in v1 (identity / placement /
/// object-store / dns have no first-class row table yet) — honest, never a
/// fabricated collection.
///
/// The version-segment choices track a standard **Kolla** catalog (Nova/Cinder/
/// Heat advertise versioned endpoint bases, so the collection is bare; Neutron/
/// Glance advertise unversioned bases, so the version rides the collection). A
/// deployment whose catalog differs surfaces an honest HTTP error, never faked
/// rows (§7) — the live path is tuned by the IAC-6 smoke.
#[must_use]
pub fn default_collection(service_type: &str) -> Option<&'static str> {
    match service_type {
        "compute" | "compute_legacy" => Some("servers/detail"),
        "network" => Some("v2.0/networks"),
        "image" => Some("v2/images"),
        "volume" | "volumev2" | "volumev3" | "block-storage" | "block-store" => {
            Some("volumes/detail")
        }
        "orchestration" | "cloudformation" => Some("stacks"),
        _ => None,
    }
}

/// A resource-list body couldn't be parsed into a table — the typed, honest
/// failure (never a fabricated empty table when the shape is unrecognized, §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceParseError(pub String);

impl std::fmt::Display for ResourceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parsing the resource list failed: {}", self.0)
    }
}

impl std::error::Error for ResourceParseError {}

/// One row in a per-service resource table (IAC-3).
///
/// `id` is the resource's `OpenStack` id — the stable selection + cross-link +
/// arming key — and `cells` are the column values aligned to the table's
/// [`ResourceTable::columns`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRow {
    /// The resource id (the row's stable key). Empty only when the API row
    /// carried no scalar `id` (honest — never invented).
    pub id: String,
    /// The value cells, one per column in [`ResourceTable::columns`] order.
    pub cells: Vec<String>,
}

/// A per-service resource table the IAC **Resources** tab renders (IAC-3).
///
/// The service type it lists, the collection queried, ordered column headers, and
/// the rows. Honest: an empty `rows` means the service genuinely has no resources
/// of this type — never a fabricated row (§7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTable {
    /// The Keystone service type this table lists (`compute`, `network`, …).
    pub service_type: String,
    /// The REST collection path queried (`servers/detail`, `stacks`, …).
    pub collection: String,
    /// The value column headers, in render order (the raw API field keys).
    pub columns: Vec<String>,
    /// The resource rows.
    pub rows: Vec<ResourceRow>,
}

impl ResourceTable {
    /// Whether the table carries no rows (the service has no resources of this
    /// type) — the honest "no resources" read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The index of the column named `header`, if present.
    #[must_use]
    pub fn column_index(&self, header: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == header)
    }

    /// The index of the table's "name" column — the first of the common name
    /// keys (`name` / `stack_name` / `display_name`) it carries, if any. Drives
    /// the row's display label + typed-arming echo.
    #[must_use]
    pub fn name_column(&self) -> Option<usize> {
        ["name", "stack_name", "display_name"]
            .iter()
            .find_map(|h| self.column_index(h))
    }

    /// The human label for `row` — its name-column cell when non-empty, else its
    /// id (never blank when the row has an id). Used for the table label, the
    /// linked view, and the typed-arming target.
    #[must_use]
    pub fn row_label<'a>(&self, row: &'a ResourceRow) -> &'a str {
        self.name_column()
            .and_then(|i| row.cells.get(i))
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&row.id)
    }

    /// Parse a standard `OpenStack` list-response `body` into a table for
    /// `service_type` / `collection`.
    ///
    /// Locates the resource array (`OpenStack` wraps it under the collection's
    /// key — `{"servers":[…]}` / `{"stacks":[…]}` / `{"networks":[…]}`), derives
    /// ordered value columns from the rows' displayable scalar/reference fields
    /// (name/status first, `id` + `links` excluded, capped at
    /// [`MAX_RESOURCE_COLUMNS`]), and builds one [`ResourceRow`] each. An empty
    /// array is a real empty table (honest "no resources"); a body with **no**
    /// recognizable array is a [`ResourceParseError`] — so a wrong endpoint /
    /// error body surfaces honestly rather than as fake-empty (§7).
    ///
    /// # Errors
    /// [`ResourceParseError`] when the body isn't valid JSON or carries no
    /// resource array.
    pub fn from_collection_json(
        service_type: &str,
        collection: &str,
        body: &str,
    ) -> Result<Self, ResourceParseError> {
        let value: serde_json::Value =
            serde_json::from_str(body.trim()).map_err(|e| ResourceParseError(e.to_string()))?;
        let rows_val = locate_resource_array(&value, collection).ok_or_else(|| {
            ResourceParseError(format!(
                "no resource array found for collection `{collection}`"
            ))
        })?;

        // Ordered value columns: every displayable field seen across the rows,
        // first-seen order, `id`/`links` excluded, name/status hoisted first,
        // capped so the table stays readable.
        let mut columns: Vec<String> = Vec::new();
        for item in rows_val {
            let Some(obj) = item.as_object() else {
                continue;
            };
            for (k, v) in obj {
                if k == "id" || k == "ID" || k == "links" {
                    continue;
                }
                if is_displayable(v) && !columns.iter().any(|c| c == k) {
                    columns.push(k.clone());
                }
            }
        }
        order_resource_columns(&mut columns);
        columns.truncate(MAX_RESOURCE_COLUMNS);

        let rows = rows_val
            .iter()
            .filter_map(|item| {
                let obj = item.as_object()?;
                let id = obj
                    .get("id")
                    .or_else(|| obj.get("ID"))
                    .and_then(display_scalar)
                    .unwrap_or_default();
                let cells = columns
                    .iter()
                    .map(|col| obj.get(col).map(display_value).unwrap_or_default())
                    .collect();
                Some(ResourceRow { id, cells })
            })
            .collect();

        Ok(Self {
            service_type: service_type.to_string(),
            collection: collection.to_string(),
            columns,
            rows,
        })
    }
}

/// Locate the resource array inside a standard list response: the collection's
/// first/last path segment key (`servers/detail` → `servers`; `v2.0/networks` →
/// `networks`), else a top-level array, else the first array-valued field. `None`
/// when the body carries no array (an error/HTML body → an honest parse failure).
fn locate_resource_array<'a>(
    value: &'a serde_json::Value,
    collection: &str,
) -> Option<&'a Vec<serde_json::Value>> {
    let first = collection.split('/').next().unwrap_or(collection);
    let last = collection.rsplit('/').next().unwrap_or(collection);
    for key in [first, last] {
        if let Some(arr) = value.get(key).and_then(|v| v.as_array()) {
            return Some(arr);
        }
    }
    if let Some(arr) = value.as_array() {
        return Some(arr);
    }
    value.as_object()?.values().find_map(|v| v.as_array())
}

/// Whether a JSON value renders as a single table cell: a scalar, or a nested
/// object that carries an `id`/`name` reference (a flavor/image ref). Arrays +
/// bare nested objects are skipped (kept out of the columns, honest — not
/// crammed into a cell).
fn is_displayable(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Bool(_) => true,
        serde_json::Value::Object(o) => o.contains_key("id") || o.contains_key("name"),
        _ => false,
    }
}

/// Render a JSON value to a table cell: a scalar to its text, a reference object
/// to its `name` (else `id`), anything else to empty (never a raw JSON blob).
fn display_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Object(o) => o
            .get("name")
            .and_then(|x| x.as_str())
            .or_else(|| o.get("id").and_then(|x| x.as_str()))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Render a scalar JSON value to its text (for the row `id`); `None` for a
/// non-scalar (an id is never a fabricated object).
fn display_scalar(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Hoist the name column first and the status column second, preserving the
/// first-seen order of the rest — so a table reads name · status · … regardless
/// of the API's field order. Stable, so equal-priority columns keep insertion
/// order.
fn order_resource_columns(columns: &mut [String]) {
    columns.sort_by_key(|c| match c.as_str() {
        "name" | "stack_name" | "display_name" => 0u8,
        "status" | "stack_status" | "state" | "power_state" => 1,
        _ => 2,
    });
}

// ─────────────────────────── Heat orchestration (IAC-4) ───────────────────────────
//
// The §6 wire shapes the IAC **Heat** tab renders — the native IaC control loop
// (design #5/#6/#21). The producer is `mackesd`'s `openstack` worker driving the
// real Heat REST API through IAC-1's `ResourceApi`; these pure types + parsers
// are what both sides share, so the shell renders a stack detail / a
// preview-update diff / a reverse-generated template without ever depending on
// `mackesd`. Every parse degrades honestly (an absent field → an empty section,
// never a fabricated resource/event/output, §7).

/// One resource of a Heat stack — the stack-detail **resources** drill (design
/// #6 "events + resources drill").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatResource {
    /// The logical resource name in the template (`resource_name`).
    pub name: String,
    /// The resource type (`OS::Nova::Server`, `OS::Neutron::Net`, …).
    pub resource_type: String,
    /// The resource's current status (`CREATE_COMPLETE`, `UPDATE_FAILED`, …).
    pub status: String,
    /// The physical (real cloud) id Heat provisioned, when the row carried one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub physical_id: String,
}

/// One event in a Heat stack's **events** timeline (design #6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatEvent {
    /// The event timestamp (`event_time`, RFC3339).
    pub time: String,
    /// The resource the event is about (`resource_name`).
    pub resource: String,
    /// The status the resource moved to (`resource_status`).
    pub status: String,
    /// The human reason (`resource_status_reason`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// One **output** of a Heat stack (design #6 "outputs").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatOutput {
    /// The output key (`output_key`).
    pub key: String,
    /// The output value rendered to a string (`output_value`).
    pub value: String,
    /// The output's description, when the template documented one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A Heat stack's full detail — the `heat-show` read (design #6): its status,
/// **resources**, **events**, **outputs**, and the **template** (HOT, read view).
///
/// `resources`/`events`/`outputs`/`template` are folded from their own Heat
/// sub-endpoint bodies; each is honestly empty when its sub-request returned
/// nothing (never a fabricated row, §7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatStackDetail {
    /// The stack's name.
    pub stack_name: String,
    /// The stack's id.
    pub stack_id: String,
    /// The stack status (`CREATE_COMPLETE`, `UPDATE_IN_PROGRESS`, …).
    pub status: String,
    /// The status reason, when Heat reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// The last-updated time (`updated_time`), when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// The stack description from the template, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The stack's resources.
    pub resources: Vec<HeatResource>,
    /// The stack's event timeline (newest first, as Heat returns it).
    pub events: Vec<HeatEvent>,
    /// The stack's outputs.
    pub outputs: Vec<HeatOutput>,
    /// The stack's HOT template, pretty-printed for the read view + the editable
    /// buffer. Empty when the template sub-request returned nothing.
    pub template: String,
}

impl HeatStackDetail {
    /// Parse a Heat `GET /stacks/{id}` response body (`{"stack": {…}}`) into the
    /// detail skeleton (name / id / status / reason / updated / description /
    /// outputs). The resources / events / template sections are folded in from
    /// their own sub-endpoint bodies via [`Self::with_resources_json`] /
    /// [`Self::with_events_json`] / [`Self::with_template_json`].
    ///
    /// # Errors
    /// [`ResourceParseError`] when the body isn't valid JSON or carries no
    /// `stack` object — so an error/HTML body surfaces honestly, never as a
    /// fabricated empty stack (§7).
    pub fn from_stack_json(body: &str) -> Result<Self, ResourceParseError> {
        let value: serde_json::Value =
            serde_json::from_str(body.trim()).map_err(|e| ResourceParseError(e.to_string()))?;
        let stack = value
            .get("stack")
            .and_then(|s| s.as_object())
            .ok_or_else(|| ResourceParseError("no `stack` object in the response".to_string()))?;
        let s = |k: &str| -> String { stack.get(k).and_then(display_scalar).unwrap_or_default() };
        let opt = |k: &str| -> Option<String> {
            stack
                .get(k)
                .and_then(display_scalar)
                .filter(|v| !v.trim().is_empty())
        };
        let outputs = stack
            .get("outputs")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        let obj = o.as_object()?;
                        Some(HeatOutput {
                            key: obj
                                .get("output_key")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                            value: obj
                                .get("output_value")
                                .map(display_value)
                                .unwrap_or_default(),
                            description: obj
                                .get("description")
                                .and_then(|d| d.as_str())
                                .map(str::to_string)
                                .filter(|d| !d.trim().is_empty()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            stack_name: s("stack_name"),
            stack_id: s("id"),
            status: s("stack_status"),
            status_reason: opt("stack_status_reason"),
            updated: opt("updated_time"),
            description: opt("description"),
            resources: Vec::new(),
            events: Vec::new(),
            outputs,
            template: String::new(),
        })
    }

    /// Fold a Heat `GET …/resources` body (`{"resources": [...]}`) into the
    /// detail — best-effort (an unparseable body leaves the resources empty, an
    /// honest "none returned" rather than an error that would hide the rest of
    /// the stack).
    #[must_use]
    pub fn with_resources_json(mut self, body: &str) -> Self {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim()) {
            if let Some(arr) = value.get("resources").and_then(|v| v.as_array()) {
                self.resources = arr
                    .iter()
                    .filter_map(|r| {
                        let o = r.as_object()?;
                        Some(HeatResource {
                            name: o
                                .get("resource_name")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                            resource_type: o
                                .get("resource_type")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                            status: o
                                .get("resource_status")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                            physical_id: o
                                .get("physical_resource_id")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                        })
                    })
                    .collect();
            }
        }
        self
    }

    /// Fold a Heat `GET …/events` body (`{"events": [...]}`) into the detail —
    /// best-effort.
    #[must_use]
    pub fn with_events_json(mut self, body: &str) -> Self {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim()) {
            if let Some(arr) = value.get("events").and_then(|v| v.as_array()) {
                self.events = arr
                    .iter()
                    .filter_map(|e| {
                        let o = e.as_object()?;
                        Some(HeatEvent {
                            time: o
                                .get("event_time")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                            resource: o
                                .get("resource_name")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                            status: o
                                .get("resource_status")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                            reason: o
                                .get("resource_status_reason")
                                .and_then(display_scalar)
                                .unwrap_or_default(),
                        })
                    })
                    .collect();
            }
        }
        self
    }

    /// Fold a Heat `GET …/template` body (the raw HOT template object) into the
    /// detail, pretty-printed for the read view + the editable buffer. A
    /// non-object body is kept verbatim; an empty body leaves the template empty.
    #[must_use]
    pub fn with_template_json(mut self, body: &str) -> Self {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return self;
        }
        self.template = serde_json::from_str::<serde_json::Value>(trimmed).map_or_else(
            |_| trimmed.to_string(),
            |v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| trimmed.to_string()),
        );
        self
    }
}

/// A Heat **preview-update** resource-change diff — the dry-run of what a
/// template edit *would* change before it is applied (design #6 preview-update).
///
/// Each field lists the logical resource names in that change class; `unchanged`
/// is carried so the diff can show the full picture, but [`Self::change_count`]
/// counts only the real changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatPreview {
    /// Resources the update would add.
    pub added: Vec<String>,
    /// Resources the update would delete.
    pub deleted: Vec<String>,
    /// Resources the update would replace (delete + recreate).
    pub replaced: Vec<String>,
    /// Resources the update would update in place.
    pub updated: Vec<String>,
    /// Resources the update would leave unchanged.
    pub unchanged: Vec<String>,
}

impl HeatPreview {
    /// Parse a Heat preview-update response (`{"resource_changes": {…}}`) into
    /// the diff. Each class's entries are the resources' logical names
    /// (`resource_name`, falling back to `resource_identity.stack_name` or a
    /// bare string). A body with no `resource_changes` is an honest no-change
    /// diff (never an error — Heat omits the key when nothing changes).
    ///
    /// # Errors
    /// [`ResourceParseError`] when the body isn't valid JSON.
    pub fn from_json(body: &str) -> Result<Self, ResourceParseError> {
        let value: serde_json::Value =
            serde_json::from_str(body.trim()).map_err(|e| ResourceParseError(e.to_string()))?;
        let changes = value.get("resource_changes");
        let names = |key: &str| -> Vec<String> {
            changes
                .and_then(|c| c.get(key))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(preview_change_name).collect())
                .unwrap_or_default()
        };
        Ok(Self {
            added: names("added"),
            deleted: names("deleted"),
            replaced: names("replaced"),
            updated: names("updated"),
            unchanged: names("unchanged"),
        })
    }

    /// How many resources the update would actually change (added + deleted +
    /// replaced + updated — `unchanged` excluded).
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.added.len() + self.deleted.len() + self.replaced.len() + self.updated.len()
    }

    /// Whether the preview shows no real change (an honest "nothing to do").
    #[must_use]
    pub fn is_no_change(&self) -> bool {
        self.change_count() == 0
    }
}

/// The logical name of one preview resource-change entry — its `resource_name`,
/// else its nested `resource_identity.stack_name`, else a bare string entry.
fn preview_change_name(entry: &serde_json::Value) -> Option<String> {
    if let Some(s) = entry.as_str() {
        return Some(s.to_string());
    }
    let o = entry.as_object()?;
    o.get("resource_name")
        .and_then(display_scalar)
        .or_else(|| {
            o.get("resource_identity")
                .and_then(|ri| ri.get("stack_name"))
                .and_then(display_scalar)
        })
        .filter(|s| !s.trim().is_empty())
}

/// The HOT resource `type:` a Keystone service **type** reverse-generates to
/// (design #5 reverse-generate).
///
/// `None` for a service with no faithful HOT mapping (Glance images / identity /
/// … are not first-class HOT resources) — those are honestly noted, never
/// emitted as a fabricated resource (§7).
#[must_use]
pub fn hot_resource_type(service_type: &str) -> Option<&'static str> {
    match service_type {
        "compute" | "compute_legacy" => Some("OS::Nova::Server"),
        "network" => Some("OS::Neutron::Net"),
        "volume" | "volumev2" | "volumev3" | "block-storage" | "block-store" => {
            Some("OS::Cinder::Volume")
        }
        "orchestration" | "cloudformation" => Some("OS::Heat::Stack"),
        _ => None,
    }
}

/// The `heat_template_version` the reverse-generator stamps (a recent stable HOT
/// version).
pub const HOT_TEMPLATE_VERSION: &str = "2021-04-16";

/// Reverse-generate a **HOT template** from live discovered resources (design #5
/// — "capture reality as code").
///
/// Emits a valid HOT skeleton: the version stamp, a description that names its
/// provenance + honesty caveat, and a `resources:` map with one entry per
/// discovered resource — keyed by a YAML-safe form of its name, `type:` mapped
/// from the service via [`hot_resource_type`], and its observed identifying value
/// as a `name` property (other observed cells ride as `#` comments, since a
/// status/address is not a settable property). A service with no HOT mapping is
/// listed honestly in a trailing comment rather than emitted as a fabricated
/// resource (§7) — the output is a review-before-apply starting point.
#[must_use]
pub fn reverse_generate_hot(tables: &[ResourceTable]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "heat_template_version: {HOT_TEMPLATE_VERSION}");
    out.push('\n');
    out.push_str(
        "description: >-\n  Reverse-generated from live infrastructure by MCNF Infra-as-Code \
         (capture\n  reality as code). Property fidelity is best-effort \u{2014} review before \
         applying.\n\n",
    );
    out.push_str("resources:\n");

    let mut emitted = 0usize;
    let mut used_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut skipped: Vec<&str> = Vec::new();
    for table in tables {
        let Some(hot_type) = hot_resource_type(&table.service_type) else {
            if !table.rows.is_empty() {
                skipped.push(table.service_type.as_str());
            }
            continue;
        };
        for row in &table.rows {
            let label = table.row_label(row);
            let key = unique_yaml_key(label, row, &mut used_keys);
            let _ = writeln!(out, "  {key}:");
            let _ = writeln!(out, "    type: {hot_type}");
            out.push_str("    properties:\n");
            let _ = writeln!(out, "      name: {}", yaml_scalar(label));
            // Other observed cells ride as reference comments (not settable props).
            for (col, cell) in table.columns.iter().zip(row.cells.iter()) {
                if col == "name" || col == "stack_name" || col == "display_name" || cell.is_empty()
                {
                    continue;
                }
                let _ = writeln!(out, "      # observed {col}: {cell}");
            }
            emitted += 1;
        }
    }
    if emitted == 0 {
        out.push_str("  {}  # no reverse-generable resources were discovered\n");
    }
    if !skipped.is_empty() {
        skipped.sort_unstable();
        skipped.dedup();
        out.push('\n');
        let _ = writeln!(
            out,
            "# discovered but not HOT-mappable (omitted, not fabricated): {}",
            skipped.join(", ")
        );
    }
    out
}

/// A YAML-safe, unique resource key derived from a resource label: non-alnum
/// runs collapse to `_`, and a numeric suffix disambiguates a collision.
fn unique_yaml_key(
    label: &str,
    row: &ResourceRow,
    used: &mut std::collections::BTreeSet<String>,
) -> String {
    let mut base: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    base = base.trim_matches('_').to_string();
    if base.is_empty() {
        base = if row.id.is_empty() {
            "resource".to_string()
        } else {
            format!(
                "res_{}",
                row.id.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
            )
        };
    }
    let mut key = base.clone();
    let mut n = 1;
    while !used.insert(key.clone()) {
        n += 1;
        key = format!("{base}_{n}");
    }
    key
}

/// Quote a scalar for a YAML value when it needs it (contains a `:` or leading/
/// trailing space); a plain token is emitted bare.
fn yaml_scalar(value: &str) -> String {
    if value.is_empty() || value.contains(':') || value.contains('#') || value != value.trim() {
        format!("{value:?}")
    } else {
        value.to_string()
    }
}

// ─────────────────────── cloud lifecycle command surface ───────────────────────
//
// The provider-neutral lifecycle-verb namespace + the typed instance/reply shapes
// the fleet cloud-lifecycle command surface rides (the `action/cloud/*` request +
// `reply/<ulid>` lane). These are the §6 wire contract between a lifecycle-command
// producer (e.g. the KDC-MESH-8 phone command surface) and the cloud backend that
// answers them; the live backend is provided by a later local-first worker.

/// The Bus topic prefix every cloud action verb rides: `action/cloud/`.
pub const CLOUD_ACTION_PREFIX: &str = "action/cloud/";

/// Current version of every `action/cloud/*` JSON request envelope.
///
/// Producers include it explicitly; consumers reject unknown future versions
/// while an omitted field decodes as the legacy v1 shape during the cutover.
pub const CLOUD_ACTION_SCHEMA_VERSION: u16 = 1;

/// systemd credential name used by the root Workloads shell and `mackesd`.
/// The credential contains exactly 64 lowercase hexadecimal characters (32 bytes).
pub const CLOUD_ARM_CREDENTIAL: &str = "cloud-arm-key";

/// HMAC-SHA256 key size accepted by the production credential loader.
pub const CLOUD_ARM_KEY_BYTES: usize = 32;

/// Stable capability target for node-wide provision/configure mutations.
pub const CLOUD_ARM_NODE_SCOPE: &str = "node-wide";

/// One live-mutation capability. The authenticated wire form is
/// `v2|nonce|expires_ms|verb|node|target|request_sha256|signature`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudArmedToken {
    /// CSPRNG single-use nonce consumed by the daemon's durable replay ledger.
    pub nonce: String,
    /// Wall-clock expiration in milliseconds since the Unix epoch.
    pub expires_at_ms: i64,
    /// Exact live mutation verb authorized by this capability.
    pub verb: String,
    /// Exact mesh placement node authorized by this capability.
    pub node: String,
    /// Exact workload/image target, or [`CLOUD_ARM_NODE_SCOPE`].
    pub target: String,
    /// SHA-256 of canonical request JSON after removing `armed_token`.
    pub request_sha256: String,
    /// Lowercase hexadecimal HMAC-SHA256 over the preceding fields.
    pub signature: String,
}

impl CloudArmedToken {
    /// Serialize the authenticated portion of the token.
    #[must_use]
    pub fn signing_payload(&self) -> String {
        format!(
            "v2|{}|{}|{}|{}|{}|{}",
            self.nonce, self.expires_at_ms, self.verb, self.node, self.target, self.request_sha256
        )
    }

    /// Serialize the complete token for the `armed_token` request field.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}|{}", self.signing_payload(), self.signature)
    }

    /// Create and HMAC-sign one capability for an already validated context.
    #[must_use]
    pub fn mint(
        signer: &dyn CloudTokenSigner,
        nonce: &str,
        expires_at_ms: i64,
        verb: &str,
        node: &str,
        target: &str,
        request_sha256: &str,
    ) -> Self {
        let mut token = Self {
            nonce: nonce.to_string(),
            expires_at_ms,
            verb: verb.to_string(),
            node: node.to_string(),
            target: target.to_string(),
            request_sha256: request_sha256.to_string(),
            signature: String::new(),
        };
        token.signature = signer.sign_payload(&token.signing_payload());
        token
    }

    /// Parse the strict v2 wire format, returning `None` for malformed input.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let parts: Vec<&str> = raw.trim().split('|').collect();
        if parts.len() != 8 || parts[0] != "v2" {
            return None;
        }
        Some(Self {
            nonce: parts[1].to_string(),
            expires_at_ms: parts[2].parse().ok()?,
            verb: parts[3].to_string(),
            node: parts[4].to_string(),
            target: parts[5].to_string(),
            request_sha256: parts[6].to_string(),
            signature: parts[7].to_string(),
        })
    }
}

/// Minimal signing interface shared by token producers and verifier adapters.
pub trait CloudTokenSigner {
    /// Return the lowercase hexadecimal signature for `payload`.
    fn sign_payload(&self, payload: &str) -> String;
}

/// Shared, dependency-light HMAC implementation used on both sides of the Bus.
/// Credential loading remains in the privileged binaries, not this wire crate.
#[derive(Clone)]
pub struct CloudArmSigner {
    key: Vec<u8>,
}

impl CloudArmSigner {
    /// Construct a signer from non-empty raw key bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when `key` is empty.
    pub fn new(key: impl Into<Vec<u8>>) -> Result<Self, &'static str> {
        let key = key.into();
        if key.is_empty() {
            return Err("cloud arming key is empty");
        }
        Ok(Self { key })
    }

    /// Return the lowercase hexadecimal HMAC-SHA256 for `payload`.
    #[must_use]
    pub fn sign_payload(&self, payload: &str) -> String {
        use hmac::{Hmac, Mac as _};
        use sha2::Sha256;

        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.key) else {
            return String::new();
        };
        mac.update(payload.as_bytes());
        hex_encode(&mac.finalize().into_bytes())
    }

    /// Verify a hexadecimal HMAC-SHA256 without timing-sensitive equality.
    #[must_use]
    pub fn verify_payload(&self, payload: &str, signature: &str) -> bool {
        use hmac::{Hmac, Mac as _};
        use sha2::Sha256;

        let Ok(signature) = hex_decode(signature) else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.key) else {
            return false;
        };
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature).is_ok()
    }
}

impl CloudTokenSigner for CloudArmSigner {
    fn sign_payload(&self, payload: &str) -> String {
        Self::sign_payload(self, payload)
    }
}

/// Decode the text stored in the sealed/systemd credential.
///
/// # Errors
///
/// Returns an error unless the credential is UTF-8 containing exactly 32
/// lowercase or uppercase hexadecimal bytes after surrounding whitespace.
pub fn decode_cloud_arm_credential(raw: &[u8]) -> Result<Vec<u8>, &'static str> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| "cloud arming credential is not UTF-8")?
        .trim();
    let key = hex_decode(text)?;
    if key.len() != CLOUD_ARM_KEY_BYTES {
        return Err("cloud arming credential must encode exactly 32 bytes");
    }
    Ok(key)
}

/// Hash a request object after removing its top-level `armed_token`.
///
/// Object keys are recursively sorted and array order is preserved.
///
/// # Errors
///
/// Returns an error for malformed JSON, a non-object root, or data that cannot
/// be serialized into the canonical representation.
pub fn cloud_request_digest(raw: &str) -> Result<String, &'static str> {
    use sha2::{Digest as _, Sha256};

    let mut value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| "cloud request is not valid JSON")?;
    let object = value
        .as_object_mut()
        .ok_or("cloud request JSON root is not an object")?;
    object.remove("armed_token");
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical)?;
    Ok(hex_encode(&Sha256::digest(canonical.as_bytes())))
}

fn write_canonical_json(
    value: &serde_json::Value,
    output: &mut String,
) -> Result<(), &'static str> {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => output.push_str(
            &serde_json::to_string(value).map_err(|_| "cloud request string cannot serialize")?,
        ),
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(']');
        }
        serde_json::Value::Object(values) => {
            output.push('{');
            let mut keys: Vec<&str> = values.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key)
                        .map_err(|_| "cloud request key cannot serialize")?,
                );
                output.push(':');
                write_canonical_json(&values[key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn hex_decode(text: &str) -> Result<Vec<u8>, &'static str> {
    if text.len() % 2 != 0 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("cloud arming credential/signature is not hexadecimal");
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|_| "invalid hexadecimal")?;
            u8::from_str_radix(pair, 16).map_err(|_| "invalid hexadecimal")
        })
        .collect()
}

/// The Bus topic for cloud verb `verb`: `action/cloud/<verb>`.
#[must_use]
pub fn cloud_action_topic(verb: &str) -> String {
    format!("{CLOUD_ACTION_PREFIX}{verb}")
}

/// A cloud-instance lifecycle action a typed verb drives through the backend seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    /// Start a stopped instance.
    Start,
    /// Stop a running instance.
    Stop,
    /// Reboot an instance (soft reboot) — destructive.
    Reboot,
    /// Delete an instance — destructive.
    Delete,
}

impl LifecycleAction {
    /// Map a lifecycle verb name to its action, or `None` for a non-lifecycle verb.
    #[must_use]
    pub fn from_verb(verb: &str) -> Option<Self> {
        match verb {
            "instance-start" => Some(Self::Start),
            "instance-stop" => Some(Self::Stop),
            "instance-reboot" => Some(Self::Reboot),
            "instance-delete" => Some(Self::Delete),
            _ => None,
        }
    }

    /// The lifecycle sub-verb token (`start` / `stop` / `reboot` / `delete`).
    #[must_use]
    pub const fn cli_verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Reboot => "reboot",
            Self::Delete => "delete",
        }
    }

    /// Whether performing this op is destructive (delete/reboot) — the ops that are
    /// only ever run past the typed-arming gate and are audited when performed (§7).
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Reboot | Self::Delete)
    }
}

/// A cloud instance as the backend reports it — the typed row the `list-instances`
/// verb returns (the Cloud plane's instance table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudInstance {
    /// The instance/server id (UUID).
    pub id: String,
    /// The instance name.
    pub name: String,
    /// The instance status (`ACTIVE` / `SHUTOFF` / `ERROR` / …).
    pub status: String,
    /// The flavor/size name-or-id, when the listing carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavor: Option<String>,
    /// The image name/id, when the listing carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The networks column, rendered to a string, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub networks: Option<String>,
}

/// The typed reply published to `reply/<request-ulid>` for an `action/cloud/*`
/// lifecycle/list verb — the neutral subset the fleet command surface reads.
///
/// `ok` mirrors the shared `{"ok":true}` reply convention. A rejected/gated/failed
/// request carries `error`/`gated` and no payload (§7 — no fabricated answer); a
/// richer backend may add per-verb payload fields, which serde ignores here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudReply {
    /// `true` when a payload answers the request; `false` on gate/failure/rejection.
    pub ok: bool,
    /// The verb this reply answers (echoed for the client's dispatch).
    #[serde(default)]
    pub verb: String,
    /// `list-instances` — the instance roster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<Vec<CloudInstance>>,
    /// An honest gate reason (the backend isn't in a state to serve this verb).
    /// Retry later; nothing was performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated: Option<String>,
    /// A rejection (malformed request) or a backend seam failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether a destructive op (delete/reboot) was performed + audited.
    #[serde(default)]
    pub audited: bool,
    /// `plan` — the pending-change counts (add/change/destroy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanCounts>,
    /// `output` — tofu outputs (instance roster / IPs) for a workload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<TofuOutput>>,
    /// `configure` / `container-deploy` — the Ansible run summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ansible: Option<AnsibleSummary>,
    /// `inventory` — the resolved mesh Ansible inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<Vec<InventoryHost>>,
    /// `image-build` — the image roster (name / sha256 / promoted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageRow>>,
    /// `console-attach` — the SPICE/VNC console handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<ConsoleEndpoint>,
    /// `set-desired` — echo of the accepted desired-state specs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired: Option<Vec<WorkloadSpec>>,
    /// Raw tool stdout/stderr, surfaced behind the shell's "expandable raw log"
    /// (the honest failure detail for a tofu/ansible/virsh run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_log: Option<String>,
}

// ─────────────────────────── provider-neutral aliases ───────────────────────────
//
// The `Cloud*`-prefixed names the recreated surfaces bind to. They alias the shapes
// above so a consumer imports either the bare name or the neutral `Cloud*` name
// from this one facade.

/// Provider-neutral alias for [`CatalogEndpoint`].
pub type CloudEndpoint = CatalogEndpoint;
/// Provider-neutral alias for [`CatalogService`].
pub type CloudService = CatalogService;
/// Provider-neutral alias for [`EndpointInterface`].
pub type CloudEndpointInterface = EndpointInterface;
/// Provider-neutral alias for [`HealthState`].
pub type CloudHealthState = HealthState;
/// Provider-neutral alias for [`ServiceCatalog`].
pub type CloudServiceCatalog = ServiceCatalog;
/// Provider-neutral alias for [`ServiceHealth`].
pub type CloudServiceHealth = ServiceHealth;
/// Provider-neutral alias for [`ResourceRow`].
pub type CloudResourceRow = ResourceRow;
/// Provider-neutral alias for [`ResourceTable`].
pub type CloudResourceTable = ResourceTable;
/// Provider-neutral alias for [`HeatStackDetail`].
pub type CloudStackDetail = HeatStackDetail;
/// Provider-neutral alias for [`HeatPreview`].
pub type CloudStackPreview = HeatPreview;
/// Provider-neutral alias for [`HeatResource`].
pub type CloudStackResource = HeatResource;
/// Provider-neutral alias for [`HeatEvent`].
pub type CloudStackEvent = HeatEvent;
/// Provider-neutral alias for [`HeatOutput`].
pub type CloudStackOutput = HeatOutput;
/// The cloud provider adapter that produced or will consume a shared contract.
///
/// `Openstack` remains a valid compatibility backend while the product-facing
/// cloud contract moves to this provider-neutral module path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudProviderAdapter {
    /// Generic Construct Cloud contract producer.
    ConstructCloud,
    /// Legacy installed OpenStack/Kolla adapter.
    Openstack,
    /// Simulator or fake provider used by tests and offline shell workflows.
    Simulator,
}

impl CloudProviderAdapter {
    /// Stable token used in persisted metadata and test fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConstructCloud => "construct_cloud",
            Self::Openstack => "openstack",
            Self::Simulator => "simulator",
        }
    }

    /// Product-facing label for UI and operator text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ConstructCloud => "Construct Cloud",
            Self::Openstack => "OpenStack adapter",
            Self::Simulator => "Cloud simulator",
        }
    }

    /// Whether this adapter is the compatibility OpenStack backend.
    #[must_use]
    pub const fn is_legacy_openstack(self) -> bool {
        matches!(self, Self::Openstack)
    }
}

/// Parse a provider-neutral Construct Cloud catalog, accepting the existing
/// OpenStack Keystone token response as a compatibility fallback.
///
/// Generic providers should publish the [`CloudServiceCatalog`] JSON shape
/// directly.  The fallback keeps the current OpenStack adapter and its contract
/// fixtures working while consumers migrate from `openstack::*` imports.
///
/// # Errors
/// Returns [`CatalogParseError`] when neither shape is recognized.
pub fn parse_service_catalog_json(body: &str) -> Result<CloudServiceCatalog, CatalogParseError> {
    let trimmed = body.trim();
    match serde_json::from_str::<CloudServiceCatalog>(trimmed) {
        Ok(catalog) => Ok(catalog),
        Err(generic_error) => {
            ServiceCatalog::from_keystone_token_json(trimmed).map_err(|openstack_error| {
                CatalogParseError(format!(
                    "provider catalog JSON did not match Construct Cloud catalog \
                         ({generic_error}) or OpenStack Keystone catalog ({openstack_error})"
                ))
            })
        }
    }
}

/// Parse a provider-neutral resource table, accepting an existing OpenStack
/// collection response as a compatibility fallback.
///
/// Generic providers should publish [`CloudResourceTable`] JSON directly.  Empty
/// `service_type` or `collection` fields are filled from the requested context so
/// small simulator fixtures can stay concise.
///
/// # Errors
/// Returns [`ResourceParseError`] when neither shape is recognized.
pub fn parse_resource_table_json(
    service_type: &str,
    collection: &str,
    body: &str,
) -> Result<CloudResourceTable, ResourceParseError> {
    let trimmed = body.trim();
    match serde_json::from_str::<CloudResourceTable>(trimmed) {
        Ok(mut table) => {
            if table.service_type.trim().is_empty() {
                table.service_type = service_type.to_string();
            }
            if table.collection.trim().is_empty() {
                table.collection = collection.to_string();
            }
            Ok(table)
        }
        Err(generic_error) => {
            ResourceTable::from_collection_json(service_type, collection, trimmed).map_err(
                |openstack_error| {
                    ResourceParseError(format!(
                        "provider resource table JSON did not match Construct Cloud table \
                         ({generic_error}) or OpenStack collection ({openstack_error})"
                    ))
                },
            )
        }
    }
}

/// Provider-neutral name for the currently supported default collection mapper.
#[must_use]
pub fn default_resource_collection(service_type: &str) -> Option<&'static str> {
    default_collection(service_type)
}

// ─────────────────── the per-node cloud backend status mirror ───────────────────
//
// WL-ARCH-001 Phase B — the provider-neutral `state/cloud/<node>` mirror the
// mackesd cloud worker (OpenTofu provision + Ansible configure over local
// libvirt/KVM) publishes and the recreated IaC surface (Phase C) consumes.
// Composed ENTIRELY of the neutral shapes above — [`CloudProviderAdapter`] (which
// backend), [`ServiceHealth`] (per-tool backend health), [`ResourceTable`] (the
// resource roster) — so the surface renders provider health + a resource table
// without ever depending on `mackesd` (the §6 layered-tiers boundary).

/// The Bus topic prefix every per-node cloud status mirror rides: `state/cloud/`.
pub const CLOUD_STATE_PREFIX: &str = "state/cloud/";

/// The per-node cloud status mirror topic: `state/cloud/<node>`.
#[must_use]
pub fn cloud_state_topic(node: &str) -> String {
    format!("{CLOUD_STATE_PREFIX}{node}")
}

/// The per-node cloud backend status the mackesd cloud worker publishes on
/// [`cloud_state_topic`].
///
/// Honest by construction (§7): `health` reports whether each toolchain leg
/// (OpenTofu / Ansible / libvirt) is actually present + reachable — an absent
/// tool reads [`HealthState::Absent`], never a fabricated `up`; `resources`
/// carries the live roster the READ verbs discovered (an empty table is a real
/// "no instances", never invented). `apply_armed` mirrors whether the operator
/// gate (`MDE_CLOUD_APPLY=1`) is set, so the surface shows plan-only vs. live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudState {
    /// This node's id (the mirror `host` stamp + the topic namespace).
    pub host: String,
    /// Which backend adapter produced this mirror (Phase B = the
    /// [`CloudProviderAdapter::ConstructCloud`] OpenTofu+Ansible backend over
    /// local libvirt).
    pub adapter: CloudProviderAdapter,
    /// Per-tool backend health (OpenTofu / Ansible / libvirt), honestly probed.
    pub health: Vec<ServiceHealth>,
    /// The resource tables the READ verbs discovered (the instance roster, …).
    pub resources: Vec<ResourceTable>,
    /// Whether armed-token live mutation is available on this node (capability,
    /// not the retired `MDE_CLOUD_APPLY` env wall). `false` ⇒ this node cannot
    /// currently honor an armed provision/configure/destroy.
    pub apply_armed: bool,
    /// Wall-clock publish time (ms since the Unix epoch).
    pub published_at_ms: i64,
    /// Per-workload rows folded from this placement node's live domains + the
    /// node's desired-state slice. Empty until the workloads engine populates it.
    #[serde(default)]
    pub workloads: Vec<WorkloadRow>,
    /// Drift rollup for this node (count of drifted workloads + last plan time).
    #[serde(default)]
    pub drift_summary: DriftSummary,
    /// This node's compute capacity — feeds the placement picker's capacity bars.
    #[serde(default)]
    pub node_capacity: NodeCapacity,
}

impl CloudState {
    /// The health row for a named backend tool (`opentofu` / `ansible` /
    /// `libvirt`), if this mirror carries one.
    #[must_use]
    pub fn tool_health(&self, service_type: &str) -> Option<&ServiceHealth> {
        self.health.iter().find(|h| h.service_type == service_type)
    }

    /// Whether every backend tool this mirror reports is healthy (`Up`) — the
    /// honest "the backend is ready to provision" read. `false` when any tool is
    /// Down/Absent, or when no tool is reported at all.
    #[must_use]
    pub fn backend_ready(&self) -> bool {
        !self.health.is_empty() && self.health.iter().all(|h| h.state == HealthState::Up)
    }
}

// ─────────────────────────── Workloads cockpit contract ───────────────────────────
//
// The neutral verb vocabulary + payload shapes the reenvisioned "Workloads" surface
// and the mackesd cloud worker exchange over `action/cloud/*` + `reply/<ulid>`. These
// extend the lifecycle/list contract above; the worker answers them, the shell renders
// them. Provider-neutral by construction (OpenTofu + Ansible + libvirt, no OpenStack).

/// `set-desired` — write a node's desired-state workload doc (`/mcnf/cloud/desired/…`).
pub const VERB_SET_DESIRED: &str = "set-desired";
/// `plan` — render the node's tfvars slice and return the pending-change counts.
pub const VERB_PLAN: &str = "plan";
/// `inventory` — the resolved mesh Ansible inventory the configure leg would target.
pub const VERB_INVENTORY: &str = "inventory";
/// `output` — the tofu outputs (instance roster / IPs) for a node's workloads.
pub const VERB_OUTPUT: &str = "output";
/// `image-build` — drive a bootc/osbuild image build (+ optional promote).
pub const VERB_IMAGE_BUILD: &str = "image-build";
/// `container-deploy` — render a Quadlet unit and hand it to the container-host role.
pub const VERB_CONTAINER_DEPLOY: &str = "container-deploy";
/// `console-attach` — return a SPICE/VNC/WebRTC console handle for a workload.
pub const VERB_CONSOLE_ATTACH: &str = "console-attach";
/// `android-provision` — the two-layer Cuttlefish path (Linux VM then `cvd`).
pub const VERB_ANDROID_PROVISION: &str = "android-provision";
/// `browser-provision` — declare the dedicated Chromium browser VM workload.
pub const VERB_BROWSER_PROVISION: &str = "browser-provision";
/// `app-provision` — declare one admitted guest-owned Flatpak App VM.
pub const VERB_APP_PROVISION: &str = "app-provision";

/// What a workload *delivers* — the cockpit's primary organizing axis (delivery type
/// × placement). Each maps to a provision + configure recipe under the hood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryType {
    /// A full VM desktop delivered as a native VDI seat.
    DesktopVm,
    /// A headless VM running a service exposed on the mesh.
    ServiceVm,
    /// A VM whose individual apps are forwarded into the MDE desktop (VDI app-mode).
    AppVm,
    /// A VM providing Android via the two-layer Cuttlefish (`cvd`) backend.
    AndroidVm,
    /// A Podman/Quadlet service container (rootless by default).
    ServiceContainer,
}

impl DeliveryType {
    /// Stable token used in desired-state docs, Ansible tags, and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DesktopVm => "desktop_vm",
            Self::ServiceVm => "service_vm",
            Self::AppVm => "app_vm",
            Self::AndroidVm => "android_vm",
            Self::ServiceContainer => "service_container",
        }
    }

    /// Product-facing label for the delivery-type view header.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DesktopVm => "Desktop",
            Self::ServiceVm => "Service",
            Self::AppVm => "Application",
            Self::AndroidVm => "Android",
            Self::ServiceContainer => "Container",
        }
    }

    /// Whether this delivery type is realized as a libvirt VM (vs a container).
    #[must_use]
    pub const fn is_vm(self) -> bool {
        !matches!(self, Self::ServiceContainer)
    }
}

/// One workload's desired state — the row the GUI form (or raw-HCL escape hatch)
/// authors and `set-desired` persists into the node's desired-state doc. The worker
/// renders these into `terraform.tfvars.json` for the placement node's slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadSpec {
    /// Stable workload name (unique within the placement node).
    pub name: String,
    /// What this workload delivers.
    pub delivery_type: DeliveryType,
    /// The mesh node this workload is placed on (its host id).
    pub node: String,
    /// Virtual CPUs (VM workloads).
    #[serde(default)]
    pub vcpu: u16,
    /// Memory in MiB (VM workloads).
    #[serde(default)]
    pub memory_mb: u32,
    /// Root disk in GiB (VM workloads).
    #[serde(default)]
    pub disk_gb: u32,
    /// The base image name (convention-path auto-discovery in the libvirt pool);
    /// `None` uses the delivery type's golden default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Whether the workload gets its own isolated network segment (opt-in; default
    /// = the shared managed mesh network).
    #[serde(default)]
    pub network_isolation: bool,
    /// The raw-HCL escape hatch: operator-supplied HCL fragment merged into the
    /// rendered tfvars (validated before `tofu`). `None` = pure form authoring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_hcl: Option<String>,
    /// Guest-owned App VM identity and policy, present only for `AppVm` rows.
    /// This is a typed declaration, never a command, path, environment, or
    /// socket description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<crate::vdi_session::AppVmLaunchRequest>,
}

/// The conservative resource profile for one guest-owned Flatpak App VM.
/// Placement and admission remain the scheduler's responsibility; this profile
/// only provides the bounded baseline used by the typed cloud declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppVmProfile {
    /// Virtual CPUs allocated to the guest.
    pub vcpu: u16,
    /// Guest memory in MiB.
    pub memory_mb: u32,
    /// Root disk in GiB.
    pub disk_gb: u32,
    /// App VMs must remain on the isolated guest network segment.
    pub network_isolation: bool,
}

/// Conservative App VM resource limits. These are policy bounds, not a claim
/// that every placement node can satisfy the request; the scheduler still
/// performs node-capacity admission after this local contract passes.
pub const APP_VM_MIN_VCPU: u16 = 1;
/// Maximum virtual CPUs admitted for one App VM.
pub const APP_VM_MAX_VCPU: u16 = 8;
/// Minimum memory admitted for one App VM, in MiB.
pub const APP_VM_MIN_MEMORY_MB: u32 = 1024;
/// Maximum memory admitted for one App VM, in MiB.
pub const APP_VM_MAX_MEMORY_MB: u32 = 32 * 1024;
/// Minimum root disk admitted for one App VM, in GiB.
pub const APP_VM_MIN_DISK_GB: u32 = 16;
/// Maximum root disk admitted for one App VM, in GiB.
pub const APP_VM_MAX_DISK_GB: u32 = 256;
/// Maximum number of capabilities admitted in one App VM request.
pub const APP_VM_MAX_CAPABILITIES: usize = 8;

/// Capabilities exposed by the supported Wayland App VM profile. Host access,
/// GPU passthrough, arbitrary networking, mounts, and sockets are deliberately
/// absent: an unknown capability is not safe to infer or silently downgrade.
pub const APP_VM_ALLOWED_CAPABILITIES: &[&str] = &[
    "audio",
    "clipboard",
    "file_chooser",
    "open_uri",
    "settings",
    "notifications",
    "screen",
    "input",
];

/// Why an App VM declaration was refused before it can enter desired state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppVmAdmissionError {
    /// The requested vCPU count is outside the App VM policy range.
    VcpuOutOfRange {
        /// Requested virtual CPU count.
        requested: u16,
        /// Inclusive lower policy bound.
        min: u16,
        /// Inclusive upper policy bound.
        max: u16,
    },
    /// The requested memory is outside the App VM policy range.
    MemoryOutOfRange {
        /// Requested memory in MiB.
        requested: u32,
        /// Inclusive lower policy bound.
        min: u32,
        /// Inclusive upper policy bound.
        max: u32,
    },
    /// The requested root disk is outside the App VM policy range.
    DiskOutOfRange {
        /// Requested root disk in GiB.
        requested: u32,
        /// Inclusive lower policy bound.
        min: u32,
        /// Inclusive upper policy bound.
        max: u32,
    },
    /// App VMs may not be placed on the shared network.
    NetworkIsolationRequired,
    /// The declaration asks for more policy capabilities than this profile
    /// can audit as one bounded request.
    TooManyCapabilities {
        /// Number of requested capabilities.
        requested: usize,
        /// Maximum admitted capability count.
        max: usize,
    },
    /// The guest profile does not implement this capability safely.
    UnsupportedCapability {
        /// Capability that is outside the profile allow-list.
        capability: String,
    },
    /// The nested lifecycle request itself failed its wire-contract checks.
    InvalidRequest(crate::vdi_session::AppVmLaunchRequestError),
}

impl core::fmt::Display for AppVmAdmissionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VcpuOutOfRange {
                requested, min, max,
            } => write!(f, "vcpu {requested} is outside the admitted range {min}..={max}"),
            Self::MemoryOutOfRange {
                requested,
                min,
                max,
            } => write!(
                f,
                "memory_mb {requested} is outside the admitted range {min}..={max}"
            ),
            Self::DiskOutOfRange {
                requested, min, max
            } => write!(f, "disk_gb {requested} is outside the admitted range {min}..={max}"),
            Self::NetworkIsolationRequired => {
                f.write_str("network isolation is required for App VMs")
            }
            Self::TooManyCapabilities { requested, max } => write!(
                f,
                "requested capability count {requested} exceeds the admitted maximum {max}"
            ),
            Self::UnsupportedCapability { capability } => {
                write!(f, "capability `{capability}` is not admitted by the App VM profile")
            }
            Self::InvalidRequest(error) => write!(f, "invalid App VM request: {error}"),
        }
    }
}

impl std::error::Error for AppVmAdmissionError {}

impl Default for AppVmProfile {
    fn default() -> Self {
        Self {
            vcpu: 2,
            memory_mb: 4096,
            disk_gb: 32,
            network_isolation: true,
        }
    }
}

impl AppVmProfile {
    /// Admit one typed App VM request against this resource and capability
    /// profile. This is deliberately pure so every provisioning boundary can
    /// apply the same fail-closed policy before writing desired state.
    pub fn admit(
        &self,
        app: &crate::vdi_session::AppVmLaunchRequest,
    ) -> Result<(), AppVmAdmissionError> {
        if !(APP_VM_MIN_VCPU..=APP_VM_MAX_VCPU).contains(&self.vcpu) {
            return Err(AppVmAdmissionError::VcpuOutOfRange {
                requested: self.vcpu,
                min: APP_VM_MIN_VCPU,
                max: APP_VM_MAX_VCPU,
            });
        }
        if !(APP_VM_MIN_MEMORY_MB..=APP_VM_MAX_MEMORY_MB).contains(&self.memory_mb) {
            return Err(AppVmAdmissionError::MemoryOutOfRange {
                requested: self.memory_mb,
                min: APP_VM_MIN_MEMORY_MB,
                max: APP_VM_MAX_MEMORY_MB,
            });
        }
        if !(APP_VM_MIN_DISK_GB..=APP_VM_MAX_DISK_GB).contains(&self.disk_gb) {
            return Err(AppVmAdmissionError::DiskOutOfRange {
                requested: self.disk_gb,
                min: APP_VM_MIN_DISK_GB,
                max: APP_VM_MAX_DISK_GB,
            });
        }
        if !self.network_isolation {
            return Err(AppVmAdmissionError::NetworkIsolationRequired);
        }
        app.validate().map_err(AppVmAdmissionError::InvalidRequest)?;
        if app.requested_capabilities.len() > APP_VM_MAX_CAPABILITIES {
            return Err(AppVmAdmissionError::TooManyCapabilities {
                requested: app.requested_capabilities.len(),
                max: APP_VM_MAX_CAPABILITIES,
            });
        }
        for capability in &app.requested_capabilities {
            if !APP_VM_ALLOWED_CAPABILITIES.contains(&capability.as_str()) {
                return Err(AppVmAdmissionError::UnsupportedCapability {
                    capability: capability.clone(),
                });
            }
        }
        Ok(())
    }

    /// Turn an admitted App VM launch into the existing Workloads desired-state
    /// contract. The caller must supply an already validated request.
    #[must_use]
    pub fn workload_spec(
        self,
        node: &str,
        name: &str,
        app: crate::vdi_session::AppVmLaunchRequest,
    ) -> WorkloadSpec {
        WorkloadSpec {
            name: name.to_string(),
            delivery_type: DeliveryType::AppVm,
            node: node.to_string(),
            vcpu: self.vcpu,
            memory_mb: self.memory_mb,
            disk_gb: self.disk_gb,
            image: Some("app-vm-wayland-standard".to_owned()),
            network_isolation: self.network_isolation,
            raw_hcl: None,
            app: Some(app),
        }
    }
}

/// The baseline profile for the dedicated Browser VM.
///
/// This is a desired-state profile, not a claim that a VM is already running.
/// The typed `browser-provision` verb persists the resulting [`WorkloadSpec`];
/// the separately armed `provision` verb is responsible for asking the backend
/// to realize the node's complete desired slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserVmProfile {
    /// Virtual CPUs allocated to the Browser VM.
    pub vcpu: u16,
    /// Memory allocated to the Browser VM, in MiB.
    pub memory_mb: u32,
    /// Root disk allocated to the Browser VM, in GiB.
    pub disk_gb: u32,
}

impl Default for BrowserVmProfile {
    fn default() -> Self {
        Self {
            vcpu: 4,
            memory_mb: 8192,
            disk_gb: 64,
        }
    }
}

impl BrowserVmProfile {
    /// Turn this baseline into the existing Desktop VM desired-state contract.
    #[must_use]
    pub fn workload_spec(self, node: &str, name: &str) -> WorkloadSpec {
        WorkloadSpec {
            name: name.to_string(),
            delivery_type: DeliveryType::DesktopVm,
            node: node.to_string(),
            vcpu: self.vcpu,
            memory_mb: self.memory_mb,
            disk_gb: self.disk_gb,
            image: None,
            network_isolation: false,
            raw_hcl: None,
            app: None,
        }
    }
}

/// The `tofu plan` change counts — the lean, counts-only preview the surface renders
/// before an armed apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCounts {
    /// Resources the plan would create.
    pub add: u32,
    /// Resources the plan would update in place / replace.
    pub change: u32,
    /// Resources the plan would destroy.
    pub destroy: u32,
}

impl PlanCounts {
    /// Whether the plan is a no-op (nothing to add/change/destroy) — in sync.
    #[must_use]
    pub const fn is_noop(self) -> bool {
        self.add == 0 && self.change == 0 && self.destroy == 0
    }
}

/// The Ansible run tally the configure/container-deploy verbs return (summary +
/// changed-count, per the lean-output decision).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsibleSummary {
    /// Tasks that ran ok (unchanged).
    pub ok: u32,
    /// Tasks that reported a change.
    pub changed: u32,
    /// Tasks that failed.
    pub failed: u32,
    /// Hosts unreachable.
    pub unreachable: u32,
    /// The names of the tasks that changed (for the "what changed" line).
    #[serde(default)]
    pub changed_tasks: Vec<String>,
}

/// One resolved host in the mesh dynamic inventory (`inventory` verb) — what the live
/// inventory panel renders so the operator sees what a configure run would target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryHost {
    /// The mesh node id.
    pub id: String,
    /// The `ansible_host` address (overlay ip or id).
    pub node: String,
    /// The inventory groups this host lands in (`role_*` / `scope_*` / `delivery_*`).
    #[serde(default)]
    pub groups: Vec<String>,
    /// Whether the host holds a live keepalive lease (alive).
    pub reachable: bool,
}

/// A single `tofu output` value (`output` verb) — the instance roster / IPs a
/// workload exposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TofuOutput {
    /// The output name.
    pub name: String,
    /// The output value rendered to a string.
    pub value: String,
    /// Whether tofu marked the output sensitive (render masked).
    #[serde(default)]
    pub sensitive: bool,
}

/// One image in the libvirt pool / airgap lane (`image-build`/list) — the per-type
/// golden image roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRow {
    /// The image name (convention-path base name).
    pub name: String,
    /// The verified content hash (the SHA256 the Syncthing lane checks).
    pub sha256: String,
    /// Whether this image is the promoted (active-base) version for its type.
    pub promoted: bool,
}

/// The console protocol a `console-attach` handle speaks.
///
/// A normal libvirt VM head is `Spice`; an Android/Cuttlefish workload's screen lives
/// inside crosvm-inside-the-L1-VM (invisible to `virsh domdisplay`), so it is handed
/// back as a `Vnc`/`WebRtc` in-guest endpoint instead of a libvirt display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleProto {
    /// A libvirt SPICE display head (the standard VM/desktop path → VDI attach).
    Spice,
    /// A VNC endpoint (the Cuttlefish `--start_vnc_server` path, or a VNC head).
    Vnc,
    /// A WebRTC endpoint URL (the Cuttlefish host-orchestrator default).
    WebRtc,
}

/// The console handle a `console-attach` verb returns — bridged to the shell's
/// SPICE/VNC → VDI attach path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleEndpoint {
    /// The protocol this handle speaks.
    pub proto: ConsoleProto,
    /// The address the shell attaches to (`spice://…` / `vnc://host:port` / an
    /// https WebRTC URL), mesh-tunneled over the overlay.
    pub uri: String,
    /// An optional one-time attach ticket/token, when the head is ticketed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

/// Whether a workload's live state matches its declared desired-state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftFlag {
    /// Live == desired (last plan was a no-op).
    InSync,
    /// Live diverges from desired (last plan had changes).
    Drift,
    /// Not yet planned / indeterminate.
    #[default]
    Unknown,
}

/// One workload row in the per-node `state/cloud` mirror — the Status/Day-2 view's
/// source of truth (live metrics + drift), folded from virsh + the desired doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadRow {
    /// The workload name.
    pub name: String,
    /// What it delivers.
    pub delivery_type: DeliveryType,
    /// The placement node it runs on.
    pub node: String,
    /// The live domain/container status (`running` / `shutoff` / `paused` / …).
    pub status: String,
    /// Live CPU utilization, whole percent (0–100) — integer to keep the mirror
    /// `Eq`/deterministic over the Bus.
    #[serde(default)]
    pub cpu_pct: u16,
    /// Live memory use in MiB.
    #[serde(default)]
    pub mem_mb: u32,
    /// Allocated disk in GiB.
    #[serde(default)]
    pub disk_gb: u32,
    /// Whether the workload's `*.mesh` name resolves + overlay path is up.
    pub reachable: bool,
    /// Desired-vs-actual drift.
    #[serde(default)]
    pub drift: DriftFlag,
    /// The admitted guest application declaration for an App VM row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<crate::vdi_session::AppVmLaunchRequest>,
}

/// Drift rollup for a node's `state/cloud` mirror.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftSummary {
    /// How many of this node's workloads are drifted.
    pub drift_count: u32,
    /// Wall-clock time (ms) of the last drift plan, `0` if never planned.
    pub last_plan_ms: i64,
}

/// A placement node's compute capacity — feeds the explicit node picker's bars.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapacity {
    /// Total virtual CPUs the node can allocate.
    pub vcpu_total: u16,
    /// Virtual CPUs currently allocated to workloads.
    pub vcpu_used: u16,
    /// Total allocatable memory (MiB).
    pub mem_total_mb: u32,
    /// Memory currently allocated to workloads (MiB).
    pub mem_used_mb: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_vm_profile_defaults_to_the_arch008_baseline() {
        let profile = BrowserVmProfile::default();
        assert_eq!(profile.vcpu, 4);
        assert_eq!(profile.memory_mb, 8192);
        assert_eq!(profile.disk_gb, 64);

        let spec = profile.workload_spec("eagle", "browser-eagle");
        assert_eq!(spec.delivery_type, DeliveryType::DesktopVm);
        assert_eq!(spec.node, "eagle");
        assert_eq!(spec.name, "browser-eagle");
        assert_eq!(spec.vcpu, 4);
        assert_eq!(spec.memory_mb, 8192);
        assert_eq!(spec.disk_gb, 64);
    }

    fn app_request() -> crate::vdi_session::AppVmLaunchRequest {
        crate::vdi_session::AppVmLaunchRequest::new(
            "org.example.Writer",
            "catalog-7",
            "wayland-standard",
            vec!["audio".into(), "clipboard".into()],
            "session-7",
            true,
        )
        .unwrap()
    }

    #[test]
    fn app_vm_default_profile_is_admitted_and_isolated() {
        let profile = AppVmProfile::default();
        let request = app_request();
        assert_eq!(profile.admit(&request), Ok(()));

        let spec = profile.workload_spec("eagle", "writer", request);
        assert_eq!(spec.vcpu, 2);
        assert_eq!(spec.memory_mb, 4096);
        assert_eq!(spec.disk_gb, 32);
        assert!(spec.network_isolation);
    }

    #[test]
    fn app_vm_admission_rejects_unsafe_cpu_memory_storage_and_network() {
        let request = app_request();

        let profile = AppVmProfile {
            vcpu: 0,
            ..AppVmProfile::default()
        };
        assert!(matches!(
            profile.admit(&request),
            Err(AppVmAdmissionError::VcpuOutOfRange { requested: 0, .. })
        ));

        let profile = AppVmProfile {
            memory_mb: APP_VM_MAX_MEMORY_MB + 1,
            ..AppVmProfile::default()
        };
        assert!(matches!(
            profile.admit(&request),
            Err(AppVmAdmissionError::MemoryOutOfRange { .. })
        ));

        let profile = AppVmProfile {
            disk_gb: APP_VM_MIN_DISK_GB - 1,
            ..AppVmProfile::default()
        };
        assert!(matches!(
            profile.admit(&request),
            Err(AppVmAdmissionError::DiskOutOfRange { .. })
        ));

        let profile = AppVmProfile {
            network_isolation: false,
            ..AppVmProfile::default()
        };
        assert_eq!(
            profile.admit(&request),
            Err(AppVmAdmissionError::NetworkIsolationRequired)
        );
    }

    #[test]
    fn app_vm_admission_rejects_unsafe_capabilities_with_explicit_reasons() {
        let mut request = app_request();
        request.requested_capabilities = vec!["gpu".into()];
        assert_eq!(
            AppVmProfile::default().admit(&request),
            Err(AppVmAdmissionError::UnsupportedCapability {
                capability: "gpu".into()
            })
        );

        request.requested_capabilities = (0..=APP_VM_MAX_CAPABILITIES)
            .map(|index| format!("capability_{index}"))
            .collect();
        assert!(matches!(
            AppVmProfile::default().admit(&request),
            Err(AppVmAdmissionError::TooManyCapabilities { .. })
        ));
    }

    #[test]
    fn cloud_request_digest_is_canonical_and_excludes_only_the_token() {
        let a = r#"{"node":"eagle","form":{"image":"nginx:1","rootful":false}}"#;
        let b = r#"{"form":{"rootful":false,"image":"nginx:1"},"armed_token":"ignored","node":"eagle"}"#;
        assert_eq!(cloud_request_digest(a), cloud_request_digest(b));
        let altered = r#"{"node":"eagle","form":{"image":"nginx:1","rootful":true}}"#;
        assert_ne!(cloud_request_digest(a), cloud_request_digest(altered));
        assert!(cloud_request_digest("[]").is_err());
    }

    #[test]
    fn cloud_armed_token_signs_the_request_digest() {
        let signer = CloudArmSigner::new(b"0123456789abcdef0123456789abcdef".to_vec()).unwrap();
        let digest = cloud_request_digest(r#"{"node":"eagle"}"#).unwrap();
        let token = CloudArmedToken::mint(
            &signer,
            "nonce-12345678",
            42,
            "provision",
            "eagle",
            CLOUD_ARM_NODE_SCOPE,
            &digest,
        );
        let parsed = CloudArmedToken::parse(&token.encode()).unwrap();
        assert_eq!(parsed, token);
        assert!(signer.verify_payload(&parsed.signing_payload(), &parsed.signature));
    }

    /// A trimmed but realistic Keystone v3 token response — the shape
    /// `POST /v3/auth/tokens` returns, with a three-interface compute service, a
    /// single-interface identity service, and an image service.
    const V3_TOKEN: &str = r#"{
      "token": {
        "methods": ["password"],
        "expires_at": "2026-07-04T12:00:00.000000Z",
        "project": {"id": "p1", "name": "mesh"},
        "catalog": [
          {
            "type": "compute",
            "id": "svc-nova",
            "name": "nova",
            "endpoints": [
              {"interface": "public",   "url": "http://nova.mesh:8774/v2.1", "region": "RegionOne", "id": "e1"},
              {"interface": "internal", "url": "http://nova.mesh:8774/v2.1", "region_id": "RegionOne", "id": "e2"},
              {"interface": "admin",    "url": "http://nova.mesh:8774/v2.1", "region": "RegionOne", "id": "e3"}
            ]
          },
          {
            "type": "identity",
            "id": "svc-keystone",
            "name": "keystone",
            "endpoints": [
              {"interface": "public", "url": "http://keystone.mesh:5000/v3", "region": "RegionOne", "id": "e4"}
            ]
          },
          {
            "type": "image",
            "name": "glance",
            "endpoints": [
              {"interface": "public", "url": "http://glance.mesh:9292", "region": "RegionOne", "id": "e5"},
              {"interface": "bogus",  "url": "http://glance.mesh:9292", "region": "RegionOne", "id": "e6"},
              {"interface": "internal", "url": "", "region": "RegionOne", "id": "e7"}
            ]
          }
        ]
      }
    }"#;

    #[test]
    fn parses_the_v3_token_catalog() {
        let cat = ServiceCatalog::from_keystone_token_json(V3_TOKEN).expect("parse");
        assert_eq!(cat.services.len(), 3);
        assert_eq!(cat.region.as_deref(), Some("RegionOne"));

        let nova = cat.service("compute").expect("compute service");
        assert_eq!(nova.name.as_deref(), Some("nova"));
        assert_eq!(nova.endpoints.len(), 3);
        assert_eq!(
            nova.endpoint(EndpointInterface::Public).unwrap().url,
            "http://nova.mesh:8774/v2.1"
        );
        // region_id folds into region for the internal endpoint.
        assert_eq!(
            nova.endpoint(EndpointInterface::Internal)
                .unwrap()
                .region
                .as_deref(),
            Some("RegionOne")
        );
        assert_eq!(nova.primary_url(), Some("http://nova.mesh:8774/v2.1"));

        // Glance drops the unrecognized interface + the empty-URL endpoint, but
        // stays listed (its presence is real).
        let glance = cat.service("image").expect("image service");
        assert_eq!(glance.endpoints.len(), 1, "bogus + empty endpoints dropped");
        assert_eq!(glance.endpoints[0].interface, EndpointInterface::Public);
    }

    #[test]
    fn a_body_without_a_catalog_is_a_typed_error_not_an_empty_catalog() {
        // §7 — an absent catalog is honest failure, never a fabricated empty one.
        let err = ServiceCatalog::from_keystone_token_json(r#"{"token":{}}"#)
            .expect_err("no catalog must fail");
        assert!(err.0.contains("catalog is absent"), "{err}");
        assert!(ServiceCatalog::from_keystone_token_json("not json").is_err());
    }

    #[test]
    fn interface_round_trips_and_tolerates_case() {
        for i in EndpointInterface::ALL {
            assert_eq!(EndpointInterface::parse(i.as_str()), Some(i));
        }
        assert_eq!(
            EndpointInterface::parse("  Public "),
            Some(EndpointInterface::Public)
        );
        assert_eq!(EndpointInterface::parse("nope"), None);
    }

    #[test]
    fn health_up_parses_a_nova_version_list_microversion() {
        // Nova's root returns a `versions` array; the CURRENT entry's
        // max_version is the microversion.
        let body = r#"{"versions":[
            {"id":"v2.0","status":"SUPPORTED","version":"","min_version":""},
            {"id":"v2.1","status":"CURRENT","version":"2.1","max_version":"2.90","min_version":"2.1"}
        ]}"#;
        let outcome = ProbeOutcome::Reachable {
            http_status: 300,
            body: body.to_string(),
            elapsed_ms: 12,
        };
        let h = shape_health(
            "compute",
            EndpointInterface::Public,
            "http://nova.mesh:8774/",
            &outcome,
        );
        assert_eq!(h.state, HealthState::Up);
        assert_eq!(h.latency_ms, Some(12));
        assert_eq!(h.version_id.as_deref(), Some("v2.1"));
        assert_eq!(h.microversion.as_deref(), Some("2.90"));
    }

    #[test]
    fn health_up_parses_a_keystone_values_and_a_single_version_doc() {
        // Keystone's root nests the list under `versions.values`.
        let keystone = r#"{"versions":{"values":[
            {"id":"v3.14","status":"stable","version":"","min_version":""}
        ]}}"#;
        let h = shape_health(
            "identity",
            EndpointInterface::Public,
            "http://keystone.mesh:5000/",
            &ProbeOutcome::Reachable {
                http_status: 300,
                body: keystone.to_string(),
                elapsed_ms: 3,
            },
        );
        assert_eq!(h.state, HealthState::Up);
        assert_eq!(h.version_id.as_deref(), Some("v3.14"));
        // No max_version + empty version ⇒ no microversion (never guessed).
        assert_eq!(h.microversion, None);

        // A versioned URL returns a single `version` object.
        let single =
            r#"{"version":{"id":"v2.1","status":"CURRENT","version":"","max_version":"2.90"}}"#;
        let h2 = shape_health(
            "compute",
            EndpointInterface::Public,
            "http://nova.mesh:8774/v2.1",
            &ProbeOutcome::Reachable {
                http_status: 200,
                body: single.to_string(),
                elapsed_ms: 5,
            },
        );
        assert_eq!(h2.microversion.as_deref(), Some("2.90"));
        assert_eq!(h2.version_id.as_deref(), Some("v2.1"));
    }

    #[test]
    fn a_5xx_reads_down_and_an_unreachable_reads_down() {
        // A cataloged-but-erroring service is down, not faked up.
        let err5xx = shape_health(
            "volumev3",
            EndpointInterface::Public,
            "http://cinder.mesh:8776/",
            &ProbeOutcome::Reachable {
                http_status: 503,
                body: String::new(),
                elapsed_ms: 8,
            },
        );
        assert_eq!(err5xx.state, HealthState::Down);
        assert_eq!(err5xx.detail.as_deref(), Some("HTTP 503"));

        let down = shape_health(
            "network",
            EndpointInterface::Public,
            "http://neutron.mesh:9696/",
            &ProbeOutcome::Unreachable {
                elapsed_ms: 2000,
                reason: "connection refused".into(),
            },
        );
        assert_eq!(down.state, HealthState::Down);
        assert_eq!(down.latency_ms, Some(2000));
        assert_eq!(down.detail.as_deref(), Some("connection refused"));
        assert!(down.microversion.is_none());
    }

    #[test]
    fn a_non_version_body_yields_no_microversion_but_still_up() {
        // A service that answers but not with a version doc is up with no
        // fabricated version.
        let h = shape_health(
            "object-store",
            EndpointInterface::Public,
            "http://swift.mesh:8080/",
            &ProbeOutcome::Reachable {
                http_status: 200,
                body: "<html>ok</html>".into(),
                elapsed_ms: 4,
            },
        );
        assert_eq!(h.state, HealthState::Up);
        assert!(h.microversion.is_none());
        assert!(h.version_id.is_none());
    }

    #[test]
    fn absent_health_is_honestly_absent() {
        let a = absent_health("dns", EndpointInterface::Public);
        assert_eq!(a.state, HealthState::Absent);
        assert!(a.url.is_empty());
        assert!(a.latency_ms.is_none());
    }

    #[test]
    fn catalog_and_health_round_trip_json() {
        // The wire contract the shell deserializes.
        let cat = ServiceCatalog::from_keystone_token_json(V3_TOKEN).unwrap();
        let s = serde_json::to_string(&cat).unwrap();
        let back: ServiceCatalog = serde_json::from_str(&s).unwrap();
        assert_eq!(cat, back);

        let health = shape_health(
            "compute",
            EndpointInterface::Public,
            "http://nova.mesh:8774/",
            &ProbeOutcome::Reachable {
                http_status: 200,
                body: String::new(),
                elapsed_ms: 1,
            },
        );
        let hs = serde_json::to_string(&health).unwrap();
        let hback: ServiceHealth = serde_json::from_str(&hs).unwrap();
        assert_eq!(health, hback);
        // The `interface` serializes as the lowercase catalog token.
        assert!(hs.contains(r#""interface":"public""#));
        assert!(hs.contains(r#""state":"up""#));
    }

    // ─────────────────────────── resource tables (IAC-3) ───────────────────────────

    #[test]
    fn default_collection_covers_the_drillable_services_and_omits_the_rest() {
        assert_eq!(default_collection("compute"), Some("servers/detail"));
        assert_eq!(default_collection("network"), Some("v2.0/networks"));
        assert_eq!(default_collection("image"), Some("v2/images"));
        assert_eq!(default_collection("volumev3"), Some("volumes/detail"));
        assert_eq!(default_collection("orchestration"), Some("stacks"));
        // A service with no first-class row table degrades honestly (no collection).
        assert_eq!(default_collection("identity"), None);
        assert_eq!(default_collection("placement"), None);
    }

    #[test]
    fn parses_a_nova_server_detail_list_into_a_table() {
        // The real `GET /servers/detail` shape: id + name + status + reference
        // objects (flavor/image) + a nested addresses object (skipped, honest).
        let body = r#"{"servers":[
            {"id":"i-1","name":"web","status":"ACTIVE",
             "flavor":{"id":"m1.small","links":[]},
             "image":{"id":"ubuntu-22"},
             "addresses":{"flat":[{"addr":"10.0.0.5"}]},
             "links":[{"rel":"self","href":"http://x"}]},
            {"id":"i-2","name":"db","status":"SHUTOFF",
             "flavor":{"id":"m1.large"}}
        ]}"#;
        let t =
            ResourceTable::from_collection_json("compute", "servers/detail", body).expect("parse");
        assert_eq!(t.service_type, "compute");
        assert_eq!(t.rows.len(), 2);
        // `id`/`links`/`addresses` are excluded; name + status lead the columns.
        assert_eq!(t.columns.first().map(String::as_str), Some("name"));
        assert_eq!(t.columns.get(1).map(String::as_str), Some("status"));
        assert!(t.columns.iter().any(|c| c == "flavor"));
        assert!(!t
            .columns
            .iter()
            .any(|c| c == "id" || c == "links" || c == "addresses"));
        // The row id rides ResourceRow::id (not a cell); a reference object renders
        // its id.
        assert_eq!(t.rows[0].id, "i-1");
        let flavor_col = t.column_index("flavor").expect("flavor column");
        assert_eq!(t.rows[0].cells[flavor_col], "m1.small");
        // A missing cell (row 2 has no image) is empty, never guessed.
        let image_col = t.column_index("image").expect("image column");
        assert_eq!(t.rows[1].cells[image_col], "");
        // The label prefers the name column.
        assert_eq!(t.row_label(&t.rows[0]), "web");
    }

    #[test]
    fn parses_a_heat_stack_list_with_stack_name_and_status() {
        let body = r#"{"stacks":[
            {"id":"s-1","stack_name":"mesh-net","stack_status":"CREATE_COMPLETE","creation_time":"2026-07-05T00:00:00Z"}
        ]}"#;
        let t =
            ResourceTable::from_collection_json("orchestration", "stacks", body).expect("parse");
        assert_eq!(t.rows.len(), 1);
        // stack_name is the name column and leads; the label falls to it.
        assert_eq!(t.name_column(), t.column_index("stack_name"));
        assert_eq!(t.columns.first().map(String::as_str), Some("stack_name"));
        assert_eq!(t.row_label(&t.rows[0]), "mesh-net");
        assert_eq!(t.rows[0].id, "s-1");
    }

    #[test]
    fn an_empty_array_is_an_honest_empty_table_and_a_bad_body_is_an_error() {
        // An empty collection is a real empty table (honest "no resources").
        let empty =
            ResourceTable::from_collection_json("network", "v2.0/networks", r#"{"networks":[]}"#)
                .expect("empty parses");
        assert!(empty.is_empty());
        assert!(empty.columns.is_empty());
        // A body with no recognizable array (an error/HTML response) is a typed
        // failure — never a fabricated empty table (§7).
        assert!(ResourceTable::from_collection_json(
            "network",
            "v2.0/networks",
            "<html>404</html>"
        )
        .is_err());
        assert!(
            ResourceTable::from_collection_json(
                "compute",
                "servers",
                r#"{"itemNotFound":{"code":404}}"#
            )
            .is_err(),
            "a 404 error object carries no array"
        );
    }

    #[test]
    fn a_row_label_falls_back_to_the_id_when_unnamed() {
        // A collection whose rows carry no name column labels by id.
        let body =
            r#"{"floatingips":[{"id":"fip-1","status":"ACTIVE","floating_ip_address":"1.2.3.4"}]}"#;
        let t = ResourceTable::from_collection_json("network", "v2.0/floatingips", body)
            .expect("parse");
        assert!(t.name_column().is_none());
        assert_eq!(t.row_label(&t.rows[0]), "fip-1");
    }

    #[test]
    fn a_resource_table_round_trips_json() {
        let t = ResourceTable::from_collection_json(
            "compute",
            "servers/detail",
            r#"{"servers":[{"id":"i-1","name":"web","status":"ACTIVE"}]}"#,
        )
        .unwrap();
        let s = serde_json::to_string(&t).unwrap();
        let back: ResourceTable = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    // ─────────────────────────── Heat orchestration (IAC-4) ───────────────────────────

    #[test]
    fn parses_a_heat_stack_detail_with_outputs() {
        let body = r#"{"stack": {
            "id": "s-1", "stack_name": "mesh-net", "stack_status": "CREATE_COMPLETE",
            "stack_status_reason": "Stack CREATE completed successfully",
            "updated_time": null, "description": "the mesh overlay network",
            "outputs": [
                {"output_key": "net_id", "output_value": "n-9", "description": "the network id"},
                {"output_key": "subnet", "output_value": "10.0.0.0/24"}
            ]
        }}"#;
        let d = HeatStackDetail::from_stack_json(body).expect("parse");
        assert_eq!(d.stack_name, "mesh-net");
        assert_eq!(d.stack_id, "s-1");
        assert_eq!(d.status, "CREATE_COMPLETE");
        assert_eq!(
            d.status_reason.as_deref(),
            Some("Stack CREATE completed successfully")
        );
        // A JSON null updated_time is honestly absent, never a fabricated time.
        assert!(d.updated.is_none());
        assert_eq!(d.outputs.len(), 2);
        assert_eq!(d.outputs[0].key, "net_id");
        assert_eq!(d.outputs[0].description.as_deref(), Some("the network id"));
        assert!(d.outputs[1].description.is_none());
        // resources/events/template are empty until folded.
        assert!(d.resources.is_empty() && d.events.is_empty() && d.template.is_empty());
    }

    #[test]
    fn folds_resources_events_and_template_into_the_detail() {
        let base = HeatStackDetail::from_stack_json(
            r#"{"stack":{"id":"s-1","stack_name":"web","stack_status":"CREATE_COMPLETE"}}"#,
        )
        .unwrap();
        let d = base
            .with_resources_json(
                r#"{"resources":[
                    {"resource_name":"server","resource_type":"OS::Nova::Server",
                     "resource_status":"CREATE_COMPLETE","physical_resource_id":"i-7"}
                ]}"#,
            )
            .with_events_json(
                r#"{"events":[
                    {"event_time":"2026-07-05T00:00:00Z","resource_name":"server",
                     "resource_status":"CREATE_IN_PROGRESS","resource_status_reason":"state changed"}
                ]}"#,
            )
            .with_template_json(r#"{"heat_template_version":"2021-04-16","resources":{}}"#);
        assert_eq!(d.resources.len(), 1);
        assert_eq!(d.resources[0].resource_type, "OS::Nova::Server");
        assert_eq!(d.resources[0].physical_id, "i-7");
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.events[0].reason, "state changed");
        // The template is pretty-printed from the JSON body for the read view.
        assert!(d.template.contains("heat_template_version"));
        assert!(d.template.contains('\n'), "pretty-printed");
    }

    #[test]
    fn a_stack_body_without_a_stack_object_is_a_typed_error() {
        // §7 — an error/HTML body surfaces honestly, never a fabricated stack.
        assert!(HeatStackDetail::from_stack_json("<html>404</html>").is_err());
        assert!(HeatStackDetail::from_stack_json(r#"{"itemNotFound":{"code":404}}"#).is_err());
        // A best-effort sub-fold of garbage leaves the section empty, not a panic.
        let d = HeatStackDetail::from_stack_json(r#"{"stack":{"id":"s","stack_name":"n"}}"#)
            .unwrap()
            .with_resources_json("<html>")
            .with_events_json("nope");
        assert!(d.resources.is_empty() && d.events.is_empty());
    }

    #[test]
    fn parses_a_preview_update_diff_and_counts_changes() {
        let body = r#"{"resource_changes": {
            "added":     [{"resource_name":"new_net"}],
            "deleted":   [{"resource_name":"old_vol"}],
            "replaced":  [{"resource_name":"server"}],
            "updated":   [],
            "unchanged": [{"resource_name":"router"}, {"resource_name":"subnet"}]
        }}"#;
        let p = HeatPreview::from_json(body).expect("parse");
        assert_eq!(p.added, vec!["new_net"]);
        assert_eq!(p.deleted, vec!["old_vol"]);
        assert_eq!(p.replaced, vec!["server"]);
        assert_eq!(p.unchanged.len(), 2);
        // change_count excludes unchanged.
        assert_eq!(p.change_count(), 3);
        assert!(!p.is_no_change());
        // Heat omits resource_changes when nothing changes → an honest no-change diff.
        let none = HeatPreview::from_json("{}").expect("empty parses");
        assert!(none.is_no_change() && none.change_count() == 0);
        // A non-JSON body is a typed error, never a fabricated diff.
        assert!(HeatPreview::from_json("<html>500</html>").is_err());
    }

    #[test]
    fn reverse_generate_emits_a_hot_from_a_fixture_resource_set() {
        // #5 capture-reality-as-code: two Nova servers + a network → a real HOT.
        let compute = ResourceTable::from_collection_json(
            "compute",
            "servers/detail",
            r#"{"servers":[
                {"id":"i-1","name":"web","status":"ACTIVE"},
                {"id":"i-2","name":"db","status":"SHUTOFF"}
            ]}"#,
        )
        .unwrap();
        let network = ResourceTable::from_collection_json(
            "network",
            "v2.0/networks",
            r#"{"networks":[{"id":"n-1","name":"mesh-net","status":"ACTIVE"}]}"#,
        )
        .unwrap();
        // An image table has no HOT mapping → honestly omitted, not fabricated.
        let image = ResourceTable::from_collection_json(
            "image",
            "v2/images",
            r#"{"images":[{"id":"img-1","name":"ubuntu","status":"active"}]}"#,
        )
        .unwrap();
        let hot = reverse_generate_hot(&[compute, network, image]);
        assert!(hot.starts_with("heat_template_version: 2021-04-16"));
        assert!(hot.contains("resources:"));
        assert!(hot.contains("OS::Nova::Server"));
        assert!(hot.contains("OS::Neutron::Net"));
        // The server + network names appear as resource keys / name props.
        assert!(hot.contains("web") && hot.contains("db") && hot.contains("mesh_net"));
        // The unmappable image service is honestly noted, never emitted as a resource.
        assert!(!hot.contains("OS::Glance"));
        assert!(hot.contains("not HOT-mappable"));
        assert!(hot.contains("image"));
    }

    #[test]
    fn reverse_generate_sanitizes_and_de_dupes_resource_keys() {
        // Two rows whose names collide after sanitizing get disambiguated keys.
        let t = ResourceTable::from_collection_json(
            "compute",
            "servers/detail",
            r#"{"servers":[
                {"id":"i-1","name":"my server","status":"ACTIVE"},
                {"id":"i-2","name":"my:server","status":"ACTIVE"}
            ]}"#,
        )
        .unwrap();
        let hot = reverse_generate_hot(&[t]);
        assert!(hot.contains("  my_server:"));
        assert!(
            hot.contains("  my_server_2:"),
            "a colliding key is disambiguated"
        );
    }

    #[test]
    fn heat_detail_round_trips_json() {
        let d = HeatStackDetail::from_stack_json(
            r#"{"stack":{"id":"s","stack_name":"n","stack_status":"CREATE_COMPLETE"}}"#,
        )
        .unwrap()
        .with_resources_json(
            r#"{"resources":[{"resource_name":"r","resource_type":"OS::Nova::Server","resource_status":"OK"}]}"#,
        );
        let s = serde_json::to_string(&d).unwrap();
        let back: HeatStackDetail = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }
}
#[cfg(test)]
mod facade_tests {
    use super::*;

    #[test]
    fn provider_neutral_catalog_json_parses_without_keystone_wrapper() {
        let body = r#"{
            "region": "vehicle-lab",
            "services": [{
                "type": "compute",
                "name": "generic-compute",
                "endpoints": [{
                    "interface": "public",
                    "url": "https://cloud.mesh/compute",
                    "region": "vehicle-lab"
                }]
            }]
        }"#;

        let catalog = parse_service_catalog_json(body).expect("generic catalog parses");

        assert_eq!(catalog.region.as_deref(), Some("vehicle-lab"));
        assert_eq!(catalog.services[0].service_type, "compute");
        assert_eq!(
            catalog.services[0].primary_url(),
            Some("https://cloud.mesh/compute")
        );
    }

    #[test]
    fn openstack_catalog_payload_still_parses_through_the_cloud_facade() {
        let body = r#"{"token":{"catalog":[{
            "type":"compute",
            "name":"nova",
            "endpoints":[{
                "interface":"public",
                "url":"https://openstack.mesh/compute/v2.1",
                "region":"RegionOne"
            }]
        }]}}"#;

        let catalog = parse_service_catalog_json(body).expect("keystone fallback parses");

        assert_eq!(catalog.region.as_deref(), Some("RegionOne"));
        assert_eq!(catalog.services[0].name.as_deref(), Some("nova"));
    }

    #[test]
    fn provider_neutral_resource_table_json_parses_without_collection_wrapper() {
        let body = r#"{
            "service_type": "compute",
            "collection": "instances",
            "columns": ["name", "status"],
            "rows": [{"id": "inst-1", "cells": ["Dispatch", "running"]}]
        }"#;

        let table =
            parse_resource_table_json("compute", "instances", body).expect("generic table parses");

        assert_eq!(table.service_type, "compute");
        assert_eq!(table.collection, "instances");
        assert_eq!(table.row_label(&table.rows[0]), "Dispatch");
    }

    #[test]
    fn openstack_collection_payload_still_parses_through_the_cloud_facade() {
        let body = r#"{"servers":[{
            "id":"server-1",
            "name":"legacy-worker",
            "status":"ACTIVE"
        }]}"#;

        let table = parse_resource_table_json("compute", "servers/detail", body)
            .expect("openstack collection fallback parses");

        assert_eq!(table.service_type, "compute");
        assert_eq!(table.collection, "servers/detail");
        assert_eq!(table.row_label(&table.rows[0]), "legacy-worker");
    }

    #[test]
    fn adapter_labels_keep_openstack_as_compatibility_not_product_default() {
        assert_eq!(
            CloudProviderAdapter::ConstructCloud.as_str(),
            "construct_cloud"
        );
        assert_eq!(
            CloudProviderAdapter::ConstructCloud.label(),
            "Construct Cloud"
        );
        assert_eq!(CloudProviderAdapter::Openstack.label(), "OpenStack adapter");
        assert!(CloudProviderAdapter::Openstack.is_legacy_openstack());
        assert!(!CloudProviderAdapter::ConstructCloud.is_legacy_openstack());
    }

    #[test]
    fn default_resource_collection_is_the_provider_neutral_import_path() {
        assert_eq!(
            default_resource_collection("compute"),
            Some("servers/detail")
        );
        assert_eq!(default_resource_collection("identity"), None);
    }

    #[test]
    fn cloud_state_topic_is_the_per_node_mirror_namespace() {
        assert_eq!(cloud_state_topic("eagle"), "state/cloud/eagle");
        assert!(cloud_state_topic("x").starts_with(CLOUD_STATE_PREFIX));
    }

    #[test]
    fn cloud_state_round_trips_json_and_reads_backend_readiness_honestly() {
        // A mirror composed entirely of the neutral facade shapes: two healthy
        // tools + an absent one, plus a one-instance resource table.
        let up = |svc: &str| ServiceHealth {
            service_type: svc.to_string(),
            interface: EndpointInterface::Internal,
            url: "(local)".to_string(),
            state: HealthState::Up,
            latency_ms: Some(1),
            microversion: None,
            version_id: None,
            detail: Some("present".to_string()),
        };
        let table = parse_resource_table_json(
            "compute",
            "instances",
            r#"{"service_type":"compute","collection":"instances","columns":["name","status"],
                "rows":[{"id":"vm-1","cells":["mesh-worker","running"]}]}"#,
        )
        .expect("table");
        let state = CloudState {
            host: "eagle".to_string(),
            adapter: CloudProviderAdapter::ConstructCloud,
            health: vec![up("opentofu"), up("ansible")],
            resources: vec![table],
            apply_armed: false,
            published_at_ms: 42,
            workloads: Vec::new(),
            drift_summary: DriftSummary::default(),
            node_capacity: NodeCapacity::default(),
        };
        assert!(state.backend_ready(), "both tools Up ⇒ ready");
        assert_eq!(
            state.tool_health("opentofu").map(|h| h.state),
            Some(HealthState::Up)
        );
        assert!(state.tool_health("libvirt").is_none());

        let s = serde_json::to_string(&state).unwrap();
        let back: CloudState = serde_json::from_str(&s).unwrap();
        assert_eq!(state, back);
        assert!(s.contains(r#""adapter":"construct_cloud""#));

        // An Absent tool drops readiness (never a fabricated up).
        let mut degraded = state.clone();
        degraded.health.push(ServiceHealth {
            service_type: "libvirt".to_string(),
            interface: EndpointInterface::Internal,
            url: String::new(),
            state: HealthState::Absent,
            latency_ms: None,
            microversion: None,
            version_id: None,
            detail: Some("libvirtd not reachable".to_string()),
        });
        assert!(!degraded.backend_ready(), "an Absent tool ⇒ not ready");
    }
}

use serde::{Deserialize, Serialize};

/// Inputs fixed when a generation is created and recorded in its bundle (see the time model).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeterminismProfile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_buckets: Vec<crate::storage::InitialBucket>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_indexed_db: Vec<crate::indexed_db::InitialDatabase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_caches: Vec<crate::cache::InitialCache>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_cookies: Vec<crate::intervention::InitialCookie>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_storage: Option<crate::intervention::InitialStorage>,
    /// Lossless native DOM storage, restored before application scripts run.
    /// Mutually exclusive with the string-based `initial_storage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_dom_storage: Option<crate::dom_storage::DomStorageState>,
    /// Reconstructed windows in creation order. Their context names are
    /// `popup-1`, `popup-2`, and so on; the launch page is `page`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_contexts: Vec<InitialContext>,
    /// Name of the launch window, installed in its fresh document before scripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_window_name: Option<String>,
    /// Initial virtual date: milliseconds since the Unix epoch at virtual time 0.
    pub initial_date_ms: u64,
    pub time_zone: String,
    pub locale: String,
    pub random_seed: u64,
    pub viewport: Viewport,
    /// A touch screen: the page sees touch support, and `input` can touch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub touch: bool,
    pub software_rendering: bool,
    /// Frame interval in integer microseconds; its multiples from time 0 round up to the millisecond frame grid.
    pub frame_interval_us: u64,
    /// Tasks the engine may run before forcing virtual time forward to prevent deadlock.
    pub starvation_limit: u32,
    pub host_class: String,
    /// Windows a page opens: enrolled in the generation, or refused (a record without it refused them).
    #[serde(default, skip_serializing_if = "PopupPolicy::is_block")]
    pub popups: PopupPolicy,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialContext {
    /// Must name the launch page or an earlier reconstructed window.
    pub opener: String,
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub no_opener: bool,
    /// A window the source had closed: created at `about:blank`, then closed before any script, so the
    /// windows after it keep their identities and an opener it was is severed as it was.
    #[serde(default)]
    pub closed: bool,
}

/// What becomes of a window a page opens (`window.open`, a link or form aimed at a new window).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupPolicy {
    /// Refused, as a popup blocker refuses it: `window.open` returns `null` and the attempt is taint.
    #[default]
    Block,
    /// A context of the generation, enrolled before it runs anything: its clock, rules and evidence are
    /// the generation's (patched engine).
    Controlled,
}

impl PopupPolicy {
    pub fn is_block(&self) -> bool {
        *self == PopupPolicy::Block
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    /// Device pixel ratio × 1000.
    pub device_scale_milli: u32,
}

impl Default for DeterminismProfile {
    fn default() -> Self {
        DeterminismProfile {
            initial_buckets: Vec::new(),
            initial_caches: Vec::new(),
            initial_indexed_db: Vec::new(),
            initial_storage: None,
            initial_dom_storage: None,
            initial_contexts: Vec::new(),
            initial_window_name: None,
            initial_cookies: Vec::new(),
            initial_date_ms: 946_684_800_000, // 2000-01-01T00:00:00Z
            time_zone: "UTC".into(),
            locale: "en-US".into(),
            random_seed: 0,
            viewport: Viewport { width: 1280, height: 720, device_scale_milli: 1000 },
            touch: false,
            software_rendering: true,
            frame_interval_us: 16_667,
            starvation_limit: 1000,
            host_class: "unspecified".into(),
            popups: PopupPolicy::Block,
        }
    }
}

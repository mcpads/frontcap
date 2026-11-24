#[derive(Debug, thiserror::Error)]
pub enum ChromiumError {
    /// Observation needs the document's observer; a progress operation may wait for it to start.
    #[error("no document has started yet; advance first")]
    DocumentNotStarted,
    #[error(transparent)]
    Cdp(cdp::CdpError),
    /// The page's renderer crashed: its state is gone and nothing it was asked is answered.
    #[error("the page's renderer crashed (its state is lost; the generation cannot continue)")]
    RendererCrashed,
    #[error(transparent)]
    Pin(#[from] browser_pin::PinError),
    #[error(transparent)]
    Install(#[from] browser_pin::InstallError),
    #[error(transparent)]
    Engine(#[from] browser_pin::engine::EngineError),
    #[error(transparent)]
    Launch(#[from] browser_pin::LaunchError),
    #[error(transparent)]
    Log(#[from] frontcap_core::log::LogError),
    #[error(transparent)]
    Bundle(#[from] frontcap_core::bundle::BundleError),
    #[error(transparent)]
    Rules(#[from] frontcap_core::rules::RuleError),
    #[error(transparent)]
    Predicate(#[from] frontcap_core::predicate::PredicateError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("page script failed: {0}")]
    Script(String),
    #[error("unexpected value from the page: {0}")]
    Shape(String),
    #[error("no node matches {0}")]
    NotFound(String),
    #[error("{count} nodes match {locator}")]
    Ambiguous { locator: String, count: usize },
    #[error("not actionable: {0}")]
    NotActionable(String),
    /// A pointer target whose center is outside the viewport: the scroll of its nearest scrollable
    /// ancestor that brings that center to the viewport's center.
    #[error("not actionable: offscreen")]
    Offscreen { delta_x: i32, delta_y: i32 },
    #[error("input reached a node outside the target: {delivered_to}")]
    AtomicityViolation { delivered_to: String },
    #[error("navigation failed: {0}")]
    Navigation(String),
    #[error("navigation cancelled by a dismissed beforeunload dialog")]
    NavigationCancelled,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("page did not reach virtual time {goal_us} us within the wall-clock watchdog")]
    Unresponsive { goal_us: u64 },
    #[error("page {target} could not join controlled execution: {detail}")]
    ContextEnrollment { target: String, detail: String },
    #[error("worker {target} could not join controlled execution at {method}: {detail}")]
    WorkerEnrollment { target: String, method: &'static str, detail: String },
    #[error("{0} did not answer within the call timeout")]
    CallTimeout(&'static str),
    #[error("no held request {0}")]
    NoSuchHold(String),
    #[error("no context {0}")]
    NoSuchContext(String),
    #[error("context {0} is closed")]
    ContextClosed(String),
    #[error("reference {0} no longer names a node of the current document")]
    StaleRef(String),
    #[error("reference {0} was issued by another generation; a reference is valid only in the generation that issued it")]
    ForeignRef(String),
    #[error("observation {0} is not one of the current document's last 16")]
    StaleCursor(u64),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("no frame has been rendered yet")]
    NoFrame,
    #[error("the last rendered frame shows an earlier document")]
    StaleFrame,
    /// What changed: the boundary's differing fields, or what else no longer holds.
    #[error("the observed boundary is no longer current ({0}): the page changed after or while it was read; request a fresh observation")]
    ObservationChanged(String),
    /// A change's `expected` value differs from the value at the boundary.
    #[error("{0}")]
    NotExpected(String),
    /// A read the page's own unfinished work holds (an IndexedDB write or version change).
    #[error("{0}")]
    PageWorkPending(String),
    /// A storage read named a database or store that does not exist.
    #[error("{0}")]
    StorageAbsent(String),
    #[error("state intervention outcome is uncertain: {0}")]
    InterventionUncertain(String),
    #[error("native input outcome is uncertain: {0}")]
    InputUncertain(String),
}

impl From<cdp::CdpError> for ChromiumError {
    fn from(e: cdp::CdpError) -> Self {
        match e {
            cdp::CdpError::TargetCrashed => ChromiumError::RendererCrashed,
            e => ChromiumError::Cdp(e),
        }
    }
}

impl ChromiumError {
    /// The host refused the engine's sandbox: its setup must change, not the launch inputs.
    pub fn sandbox_restricted(&self) -> bool {
        matches!(self, ChromiumError::Launch(browser_pin::LaunchError::SandboxRestricted))
    }
}

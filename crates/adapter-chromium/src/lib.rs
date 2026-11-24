//! One Chromium generation driven under frontcap's virtual-time contract.

pub mod bindings;
pub mod encode;
pub mod dialog_control;
pub mod frames;
pub mod error;
pub mod generation;
pub mod input;
pub mod instrument;
pub mod keys;
pub mod network;
pub mod observe;
pub mod session;
pub mod service_clock;
pub mod storage_metadata;
pub mod dom_storage;

pub use error::ChromiumError;
pub use generation::{Generation, LaunchOptions};

mod rule_history;

//! Durable request admission for the experimental native adapter.
//!
//! Typed messages connect the session owner to the Python client bridge.
//! The experimental native recorder hosts this owner; the MCP native tool routes to that recorder.
//! The caller must hold target ownership;
//! this store also holds an exclusive file lock so two bridge owners cannot allocate requests.
pub mod bridge;
pub mod request;
pub mod store;
pub mod external_entry;
#[cfg(unix)]
pub mod android_app_state;

#[cfg(unix)]
pub mod process;

#[cfg(unix)]
pub mod resource_process;

#[cfg(unix)]
pub mod diagnostic_log;
pub mod worker_control;

#[cfg(unix)]
pub mod resource_group;

#[cfg(unix)]
pub mod storage_volume;
mod emulator_confinement;

#[cfg(unix)]
mod resource_handle;

#[cfg(unix)]
pub mod worker;

#[cfg(unix)]
pub mod device;

#[cfg(unix)]
mod emulator_disks;
mod avd_copy;

#[cfg(unix)]
pub mod session;


pub mod receipt;

pub mod ios;

#[cfg(unix)]
mod host;
#[cfg(unix)]
pub mod simulator;
#[cfg(unix)]
pub mod products;
#[cfg(unix)]
pub mod xctest;

#[cfg(unix)]
pub mod ios_observation;

pub mod ios_store;

#[cfg(unix)]
pub mod ios_receipt;

#[cfg(unix)]
pub mod ios_execution;

#[cfg(unix)]
pub mod ios_target;
#[cfg(unix)]
pub mod ios_restarts;
pub mod ios_app_state;

#[cfg(unix)]
mod ios_external_entry;

#[cfg(unix)]
pub mod ios_session;

pub mod flutter_store;

#[cfg(unix)]
pub mod flutter_bridge;

#[cfg(unix)]
pub mod flutter_session;
pub mod flutter_ios_app;
pub mod flutter_ios_entry;
#[cfg(test)]
mod flutter_ios_fixture;
pub mod flutter_android_app;
pub mod flutter_app;
pub mod flutter_app_state;
pub mod flutter_restarts;

#[cfg(unix)]
pub mod flutter_target;
#[cfg(unix)]
pub mod flutter_system_ui;
pub mod ios_system_store;
pub mod ios_system_runner;


pub mod android_capture;

pub mod android_collection;

pub mod android_app;
pub mod android_restarts;
pub(crate) mod android_installation;
pub(crate) mod android_app_data;

mod android_control;

pub mod webview;
pub mod webview_store;
pub mod webview_lifecycle_store;
#[cfg(unix)]
pub mod simulator_app;
#[cfg(unix)]
pub mod webview_target;
#[cfg(unix)]
pub mod webview_session;

#[cfg(unix)]
pub mod contact_session;

mod loopback;

#[cfg(unix)]
pub mod android_webview;

#[cfg(unix)]
pub mod android_webview_page;
mod android_console;
mod android_navigation;

#[cfg(unix)]
pub mod android_frames;

#[cfg(unix)]
pub mod android_timers;

pub mod android_pointers;

pub mod android_webview_session;

#[cfg(unix)]
mod simulator_input;
#[cfg(unix)]
pub mod ios_host;
mod ios_host_pages;
mod ios_host_fault;
#[cfg(unix)]
pub mod ios_host_session;

#[cfg(unix)]
pub mod app_data;

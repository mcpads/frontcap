//! Typed native request and receipt data shared by adapters and control protocols.
//! Android fixture operations, iOS clock/UI boundaries, and Flutter control data.
pub mod request;
pub mod observation;
pub mod ownership;
pub mod ios;
pub mod ios_lifecycle;
pub mod app_state;
pub mod ios_edit;
pub mod ios_ui;


pub mod ios_request;

pub mod flutter;
pub mod flutter_lifecycle;
pub mod flutter_entry;

pub mod ios_targets;
pub mod ios_system_ui;

pub mod ios_text;

pub mod ios_press;

pub mod ios_scroll;

pub mod android_views;

pub mod android_capture;

pub mod android_app;
pub mod android_lifecycle;

pub mod android_edit;

pub mod android_tap;
pub mod android_scroll;

pub mod service;

pub mod webview;
pub mod webview_lifecycle;
pub mod webview_observation;
pub mod webview_console;
pub mod webview_media;
pub mod speech_observation;
pub mod android_speech;
pub mod webview_navigation;

pub mod contacts;

pub mod webview_progress;

pub mod android_frames;
pub mod android_timers;
pub mod android_pointers;
pub mod android_webview;
pub mod android_console;
pub mod android_navigation;

pub mod android_progress;
pub mod external_entry;

pub mod android_host;

pub mod ios_host;
pub mod ios_navigation;

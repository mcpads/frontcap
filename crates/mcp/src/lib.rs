//! The control MCP server: a stateless client of per-generation recorders.
pub mod jsonrpc;
pub mod server;
pub mod config;
pub mod tools;
pub mod connections;
pub mod dispatch;

mod discovery;
mod saved;
mod probe;
mod result_pages;
mod recordings;
mod keyframes;
mod derived;
mod video;
mod history;
pub mod native;

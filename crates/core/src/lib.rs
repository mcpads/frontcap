//! Target-independent model of frontcap: identifiers, virtual time, execution records, the request
//! journal, the agenda, quiescence, predicates, network rules, and evidence bundles.

pub mod ids;
pub mod time;
pub mod canonical;
pub mod dialog;
pub mod event;
pub mod lines;
pub mod log;
pub mod observation;
pub mod projection;
pub mod journal;
pub mod state;
pub mod write_budget;
pub mod agenda;
pub mod clock;
pub mod predicate;
pub mod action;
pub mod process;
pub mod profile;
pub mod rules;
pub mod bundle;
pub mod encode;
pub mod inputs;
pub mod cassette;

#[cfg(all(test, feature = "schema"))]
#[path = "schema_tests.rs"]
mod schema_tests;

pub mod intervention;
pub mod storage;
pub mod dom_storage;
pub mod cache;
pub mod indexed_db;
pub mod pointer;
pub mod basis;

pub mod native;

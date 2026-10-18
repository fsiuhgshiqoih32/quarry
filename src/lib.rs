//! Quarry: instant file search for Windows, macOS and Linux.
//!
//! The library holds everything but the window: the index ([`index`]), scanning ([`scan`]), the
//! query language ([`query`]), searching ([`search`]), live updates ([`engine`]), persistence
//! ([`store`]) and the HTTP API ([`serve`]).

pub mod art;
pub mod cli;
pub mod config;
pub mod engine;
pub mod fmt;
pub mod index;
pub mod query;
pub mod scan;
pub mod search;
pub mod serve;
pub mod store;

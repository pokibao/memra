//! Public re-exports for integration tests and downstream crates.
//!
//! The binary crate (`main.rs`) declares these modules privately.
//! This lib crate re-exports the subset needed by integration tests.

pub mod audit;
pub mod cli;
pub mod config;
pub mod service;
pub mod transport;

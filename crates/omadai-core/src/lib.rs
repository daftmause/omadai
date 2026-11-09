//! The omadai core: credentials, keystore, HTTP policy, config, cache, lock and report aggregation.

pub mod cache;
pub mod config;
pub mod date;
pub mod errlog;
pub mod error;
pub mod fetch;
pub mod fsutil;
pub mod google;
pub mod http;
pub mod keyring_file;
pub mod keystore;
pub mod lock;
pub mod paths;
pub mod platform;
pub mod provider;
pub mod providers;
pub mod report;
pub mod scrub;
pub mod secret;

pub use error::Error;
pub use rust_decimal::Decimal;
pub use secret::Secret;

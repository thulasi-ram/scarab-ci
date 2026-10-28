//! The first-party Argo CD Plugin (ADR-0072).
//!
//! It runs in its own Pod in the `scarab-plugins` namespace and can reach
//! only the CredentialBroker. It reads Argo CD through the broker's read
//! proxy, under the read rules `manifest.json` declares — the Argo token is
//! the broker's, never this process's (ADR-0072 §5).
//!
//! ADR-0074 removed the Deployment record this plugin used to report into.
//! What stays is library code a plugin will need to answer whether an
//! Application is live: the Argo CD API types ([`argo`]), the status rules
//! ([`assess`]), the rendered image set ([`version`]) and the broker client
//! ([`broker`]). The process itself only checks it can reach both
//! ([`standby`]).
//!
//! This crate depends on no `scarab-*` crate: what it knows of Scarab is the
//! broker's HTTP contract.

pub mod argo;
pub mod assess;
pub mod broker;
pub mod config;
pub mod standby;
pub mod version;

pub use config::Config;
pub use standby::run;

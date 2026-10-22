//! The first-party Argo CD reporter Plugin (ADR-0072), the reference
//! implementation of the DeploymentReporterContract (ADR-0070 §11).
//!
//! It runs in its own Pod in the `scarab-plugins` namespace, can reach only
//! the CredentialBroker, and does two things there:
//!
//! - **reads Argo CD** through the broker's read proxy, under the read rules
//!   `manifest.json` declares (list, watch, managed resources) — the Argo
//!   token is the broker's, never this process's (ADR-0072 §5);
//! - **pushes DeploymentStatusReports** to the broker's ingest, under the
//!   identity of its projected ServiceAccount token; core sets each report's
//!   `source` from that identity (ADR-0072 §11).
//!
//! A DeploymentTarget is an Application (`<appNamespace>/<name>`), grouped by
//! its Argo CD project; its DeploymentVersion is the set of images in its
//! target manifests' pod templates ([`version`], ADR-0070 §4, provisional);
//! its status is [`assess::assess`].
//!
//! This crate depends on no `scarab-*` crate: what it knows of Scarab is the
//! broker's HTTP contract, so it doubles as the proof that a third party can
//! write a reporter from that contract alone.

pub mod argo;
pub mod assess;
pub mod broker;
pub mod config;
pub mod report;
pub mod reporter;
pub mod version;

pub use config::Config;
pub use reporter::run;

//! ADR-0062 part 3 — settling a **change set** into the CAS.
//!
//! [`crate::changeset`] reads an `overlayfs` upper layer and answers *which paths
//! the Attempt touched*. That answer is paths only: no hashes, no metadata, no
//! snapshot. This module is the other half — it takes that change set plus the
//! parent snapshot the Export was built from and produces the **new Workspace
//! Snapshot**: a root (the address) and a content identity (what the bytes are).
//!
//! Filled in by ADR-0062 s4.

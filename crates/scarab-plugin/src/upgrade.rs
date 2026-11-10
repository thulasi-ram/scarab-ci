//! What an upgrade changes (ADR-0072 §10): the installed manifest against
//! the one an admin read for the upgrade, item by item.
//!
//! A manifest that is **wider** than the installed one is governed like an
//! install. (So is any new image digest, whatever the manifest: a new digest
//! is new code. That is the server's check on the image, not this diff's;
//! only a same-or-narrower manifest on the installed digest is exempt.) Wider
//! means
//! the new manifest asks for something the installed one did not: a
//! capability, a connection slot, an auth scheme on a slot, a read rule, or a
//! gate (a gate whose definition changed counts as added), or it stops
//! naming the image repository it named, or names another. Removing anything
//! and adding a config key are not wider: a config value is set by the
//! admin, never by the plugin. The server decides this from the two
//! manifests it holds, never from what a client says.

use crate::{PluginCapability, PluginManifest};

/// One group of a [`ManifestDiff`]: what only the new manifest has, what
/// only the installed one has, and what both have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetDiff<T> {
    pub added: Vec<T>,
    pub removed: Vec<T>,
    pub unchanged: Vec<T>,
}

impl<T: PartialEq + Clone> SetDiff<T> {
    /// `new` against `old`, each group in the order its manifest lists them.
    pub fn of(old: &[T], new: &[T]) -> Self {
        Self {
            added: new.iter().filter(|x| !old.contains(x)).cloned().collect(),
            removed: old.iter().filter(|x| !new.contains(x)).cloned().collect(),
            unchanged: new.iter().filter(|x| old.contains(x)).cloned().collect(),
        }
    }
}

impl<T> SetDiff<T> {
    pub fn is_unchanged(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// The installed manifest against a new one. Rules and auth schemes are
/// keyed by their slot (`<slot> <rule>`), so a rule moved to another slot is
/// a removal and an addition; a rule is one `(method, path, query keys)`, so
/// one more allowed query key is a new rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDiff {
    pub capabilities: SetDiff<PluginCapability>,
    /// Connection slot names.
    pub connections: SetDiff<String>,
    /// `<slot>: <scheme>`.
    pub auth_schemes: SetDiff<String>,
    /// `<slot>: <method> <path>[ ?<key>&<key>…]`.
    pub read_rules: SetDiff<String>,
    pub config_keys: SetDiff<String>,
    /// Gate names. A gate whose description or interface changed is in both
    /// `removed` and `added`.
    pub gates: SetDiff<String>,
    /// The image repository the manifest names, when it names one.
    pub image: SetDiff<String>,
}

impl ManifestDiff {
    pub fn between(old: &PluginManifest, new: &PluginManifest) -> Self {
        let slots = |m: &PluginManifest| -> Vec<String> {
            m.connections.iter().map(|s| s.name.clone()).collect()
        };
        let schemes = |m: &PluginManifest| -> Vec<String> {
            m.connections
                .iter()
                .flat_map(|s| s.auth_schemes.iter().map(|a| format!("{}: {a}", s.name)))
                .collect()
        };
        let rules = |m: &PluginManifest| -> Vec<String> {
            m.connections
                .iter()
                .flat_map(|s| {
                    s.read_rules
                        .iter()
                        .map(|r| format!("{}: {}", s.name, rule_with_query(r)))
                })
                .collect()
        };
        let image = |m: &PluginManifest| -> Vec<String> { m.image.iter().cloned().collect() };
        // A gate is its name and its whole definition: the same name with
        // another interface is not the gate an admin approved.
        let gates = SetDiff::of(
            &old.gates.iter().collect::<Vec<_>>(),
            &new.gates.iter().collect::<Vec<_>>(),
        );
        let names = |xs: Vec<(&String, &crate::GateDefinition)>| {
            xs.into_iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
        };
        Self {
            capabilities: SetDiff::of(&old.capabilities, &new.capabilities),
            connections: SetDiff::of(&slots(old), &slots(new)),
            auth_schemes: SetDiff::of(&schemes(old), &schemes(new)),
            read_rules: SetDiff::of(&rules(old), &rules(new)),
            config_keys: SetDiff::of(&old.config_keys, &new.config_keys),
            gates: SetDiff {
                added: names(gates.added),
                removed: names(gates.removed),
                unchanged: names(gates.unchanged),
            },
            image: SetDiff::of(&image(old), &image(new)),
        }
    }

    /// What the new manifest asks for that the installed one did not, each
    /// as a sentence an admin confirms. Empty = same or narrower.
    pub fn widenings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.capabilities.added {
            out.push(format!("adds capability `{c}`"));
        }
        for s in &self.connections.added {
            out.push(format!("adds connection slot `{s}`"));
        }
        for s in &self.auth_schemes.added {
            out.push(format!("adds auth scheme `{s}`"));
        }
        for r in &self.read_rules.added {
            out.push(format!("adds read rule `{r}`"));
        }
        for g in &self.gates.added {
            if self.gates.removed.contains(g) {
                out.push(format!("changes gate `{g}`"));
            } else {
                out.push(format!("adds gate `{g}`"));
            }
        }
        // Naming no image, or another, loosens which code the manifest runs
        // with; naming one where there was none narrows it.
        for i in &self.image.removed {
            match self.image.added.first() {
                Some(new) => out.push(format!("names image `{new}` instead of `{i}`")),
                None => out.push(format!("stops naming its image `{i}`")),
            }
        }
        out
    }

    /// Does the new manifest ask for anything the installed one did not?
    pub fn widens(&self) -> bool {
        !self.widenings().is_empty()
    }
}

/// A read rule with the query keys it allows: `GET /api/v1/applications
/// ?projects&selector`, as the approval screen shows it.
fn rule_with_query(r: &crate::RequestRule) -> String {
    if r.query.is_empty() {
        format!("{} {}", r.method, r.path)
    } else {
        format!("{} {} ?{}", r.method, r.path, r.query.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{ConnectionSlot, RequestRule};

    fn rule(path: &str, query: &[&str]) -> RequestRule {
        RequestRule {
            method: "GET".into(),
            path: path.into(),
            query: query.iter().map(|q| q.to_string()).collect(),
        }
    }

    fn manifest() -> PluginManifest {
        PluginManifest {
            plugin: "argocd".into(),
            image: Some("ghcr.io/acme/argocd".into()),
            capabilities: vec![
                PluginCapability::UseConnection,
                PluginCapability::ResolveGates,
            ],
            connections: vec![ConnectionSlot {
                name: "argocd".into(),
                auth_schemes: vec!["bearer".into()],
                read_rules: vec![
                    rule("/api/version", &[]),
                    rule("/api/v1/applications", &["projects"]),
                ],
            }],
            config_keys: vec!["ARGOCD_UI_URL".into()],
            gates: BTreeMap::from([(
                "wait-live".to_string(),
                serde_json::from_value(serde_json::json!({
                    "description": "Pass when live.",
                    "interface": { "inputs": [{ "name": "app", "type": "string" }] }
                }))
                .unwrap(),
            )]),
        }
    }

    #[test]
    fn the_same_manifest_changes_nothing_and_does_not_widen() {
        let d = ManifestDiff::between(&manifest(), &manifest());
        assert!(d.capabilities.is_unchanged() && d.read_rules.is_unchanged());
        assert_eq!(d.read_rules.unchanged.len(), 2);
        assert_eq!(d.gates.unchanged, vec!["wait-live".to_string()]);
        assert!(!d.widens());
    }

    #[test]
    fn removing_anything_or_adding_a_config_key_is_narrower() {
        let mut new = manifest();
        new.connections[0].read_rules.pop();
        new.config_keys = vec!["OTHER".into()];
        new.gates.clear();
        new.capabilities
            .retain(|c| *c != PluginCapability::ResolveGates);
        let d = ManifestDiff::between(&manifest(), &new);
        assert_eq!(
            d.read_rules.removed,
            vec!["argocd: GET /api/v1/applications ?projects".to_string()]
        );
        assert_eq!(d.config_keys.added, vec!["OTHER".to_string()]);
        assert_eq!(d.config_keys.removed, vec!["ARGOCD_UI_URL".to_string()]);
        assert_eq!(d.gates.removed, vec!["wait-live".to_string()]);
        assert_eq!(d.capabilities.removed, vec![PluginCapability::ResolveGates]);
        assert_eq!(d.widenings(), Vec::<String>::new());
    }

    #[test]
    fn each_kind_of_addition_widens_and_says_what_it_adds() {
        type Mutation = Box<dyn Fn(&mut PluginManifest)>;
        let cases: Vec<(Mutation, &str)> = vec![
            (
                Box::new(|m| {
                    m.connections[0]
                        .read_rules
                        .push(rule("/api/v1/account", &[]))
                }),
                "adds read rule `argocd: GET /api/v1/account`",
            ),
            (
                // One more query key is a new rule.
                Box::new(|m| m.connections[0].read_rules[1].query.push("refresh".into())),
                "adds read rule `argocd: GET /api/v1/applications ?projects&refresh`",
            ),
            (
                Box::new(|m| {
                    m.connections.push(ConnectionSlot {
                        name: "github".into(),
                        auth_schemes: vec!["bearer".into()],
                        read_rules: vec![],
                    })
                }),
                "adds connection slot `github`",
            ),
            (
                Box::new(|m| m.capabilities.push(PluginCapability::SubscribeEvents)),
                "adds capability `subscribe_events`",
            ),
            (
                Box::new(|m| {
                    let g = m.gates["wait-live"].clone();
                    m.gates.insert("wait-synced".into(), g);
                }),
                "adds gate `wait-synced`",
            ),
            (
                Box::new(|m| m.gates.get_mut("wait-live").unwrap().description = "x".into()),
                "changes gate `wait-live`",
            ),
            (
                Box::new(|m| m.image = None),
                "stops naming its image `ghcr.io/acme/argocd`",
            ),
            (
                Box::new(|m| m.image = Some("ghcr.io/evil/argocd".into())),
                "names image `ghcr.io/evil/argocd` instead of `ghcr.io/acme/argocd`",
            ),
        ];
        for (mutate, says) in cases {
            let mut new = manifest();
            mutate(&mut new);
            let d = ManifestDiff::between(&manifest(), &new);
            assert!(d.widens(), "{says}");
            assert!(
                d.widenings().iter().any(|w| w == says),
                "want `{says}` in {:?}",
                d.widenings()
            );
        }
    }

    #[test]
    fn naming_an_image_where_there_was_none_is_narrower() {
        let mut old = manifest();
        old.image = None;
        let d = ManifestDiff::between(&old, &manifest());
        assert_eq!(d.image.added, vec!["ghcr.io/acme/argocd".to_string()]);
        assert!(!d.widens());
    }
}

//! Where plugin Pods and the isolation probe's Pods schedule
//! (`SCARAB_PLUGIN_PLACEMENT`, git-bug 1b8202c).
//!
//! On a cluster whose every node is tainted, a Pod without a matching
//! toleration stays Pending, so no Plugin could start and the probe could
//! never run. The operator sets tolerations and a nodeSelector once; the
//! runtime puts them on every plugin Deployment's Pod template and on the
//! three probe Pods, so the probe tests the nodes Plugins run on. A Plugin's
//! manifest cannot ask for placement.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{PodSpec, Toleration};
use serde_json::Value;

/// The operator's placement for plugin and probe Pods. Empty = none (the
/// Pods carry no tolerations and no nodeSelector, as before).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginPlacement {
    pub tolerations: Vec<Toleration>,
    pub node_selector: BTreeMap<String, String>,
}

impl PluginPlacement {
    /// Parse `{"tolerations": [...], "nodeSelector": {...}}`, both optional.
    ///
    /// Strict, because a mistake here does not fail anything visibly: a
    /// misspelled toleration field would be dropped, the toleration would
    /// match no taint, and every plugin Pod would sit Pending. So an unknown
    /// key, at the top or in a toleration, is refused.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(raw).map_err(|e| format!("not JSON: {e}"))?;
        let Value::Object(mut top) = value else {
            return Err("must be a JSON object with `tolerations` and `nodeSelector`".into());
        };
        let tolerations = match top.remove("tolerations") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .into_iter()
                .enumerate()
                .map(|(i, item)| parse_toleration(i, item))
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("`tolerations` must be a list".into()),
        };
        let node_selector = match top.remove("nodeSelector") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(v @ Value::Object(_)) => serde_json::from_value::<BTreeMap<String, String>>(v)
                .map_err(|_| "`nodeSelector` must map label keys to string values".to_string())?,
            Some(_) => return Err("`nodeSelector` must be a map".into()),
        };
        if let Some(key) = top.keys().next() {
            return Err(format!(
                "unknown key `{key}`; only `tolerations` and `nodeSelector` are read"
            ));
        }
        if let Some(key) = node_selector.keys().find(|k| k.is_empty()) {
            return Err(format!("`nodeSelector` has an empty key ({key:?})"));
        }
        Ok(PluginPlacement {
            tolerations,
            node_selector,
        })
    }

    /// Put the placement on a Pod spec the runtime built. The runtime's
    /// builders set neither field, so nothing of theirs is overwritten.
    pub fn apply(&self, spec: &mut PodSpec) {
        if !self.tolerations.is_empty() {
            spec.tolerations = Some(self.tolerations.clone());
        }
        if !self.node_selector.is_empty() {
            spec.node_selector = Some(self.node_selector.clone());
        }
    }
}

/// One toleration, refused when it holds a key `Toleration` does not know
/// (it would be dropped silently) or a value of the wrong type.
fn parse_toleration(i: usize, item: Value) -> Result<Toleration, String> {
    let Value::Object(fields) = &item else {
        return Err(format!("tolerations[{i}] must be an object"));
    };
    const KNOWN: [&str; 5] = ["key", "operator", "value", "effect", "tolerationSeconds"];
    if let Some(unknown) = fields.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(format!(
            "tolerations[{i}] has an unknown field `{unknown}` (known: {})",
            KNOWN.join(", ")
        ));
    }
    serde_json::from_value(item).map_err(|e| format!("tolerations[{i}]: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerations_and_a_node_selector_are_read() {
        let p = PluginPlacement::parse(
            r#"{"tolerations":[{"key":"workload-type","operator":"Equal",
                "value":"application-semi-critical","effect":"NoSchedule"},
                {"key":"node.kubernetes.io/not-ready","operator":"Exists",
                "effect":"NoExecute","tolerationSeconds":300}],
                "nodeSelector":{"kubernetes.io/arch":"arm64"}}"#,
        )
        .unwrap();
        assert_eq!(p.tolerations.len(), 2);
        assert_eq!(p.tolerations[1].toleration_seconds, Some(300));
        assert_eq!(p.tolerations[0].key.as_deref(), Some("workload-type"));
        assert_eq!(p.tolerations[0].effect.as_deref(), Some("NoSchedule"));
        assert_eq!(p.node_selector["kubernetes.io/arch"], "arm64");

        let mut spec = PodSpec::default();
        p.apply(&mut spec);
        assert_eq!(spec.tolerations.as_deref(), Some(&p.tolerations[..]));
        assert_eq!(spec.node_selector.as_ref(), Some(&p.node_selector));
    }

    #[test]
    fn an_empty_placement_leaves_the_pod_alone() {
        for raw in [
            "{}",
            r#"{"tolerations":[],"nodeSelector":{}}"#,
            r#"{"tolerations":null}"#,
        ] {
            let p = PluginPlacement::parse(raw).unwrap();
            assert_eq!(p, PluginPlacement::default(), "{raw}");
            let mut spec = PodSpec::default();
            p.apply(&mut spec);
            assert_eq!(spec.tolerations, None, "{raw}");
            assert_eq!(spec.node_selector, None, "{raw}");
        }
    }

    /// A misspelled field would otherwise be dropped, leaving a toleration
    /// that matches no taint and every plugin Pod Pending.
    #[test]
    fn a_misspelled_or_unknown_field_is_refused() {
        for (raw, says) in [
            (
                r#"{"tolerations":[{"key":"k","efect":"NoSchedule"}]}"#,
                "`efect`",
            ),
            (r#"{"toleration":[]}"#, "`toleration`"),
            (r#"{"node_selector":{"a":"b"}}"#, "`node_selector`"),
            (r#"{"tolerations":{"key":"k"}}"#, "must be a list"),
            (r#"{"tolerations":["k"]}"#, "must be an object"),
            (
                r#"{"tolerations":[{"tolerationSeconds":"60"}]}"#,
                "tolerations[0]",
            ),
            (r#"{"nodeSelector":{"pool":1}}"#, "string values"),
            (r#"{"nodeSelector":{"":"x"}}"#, "empty key"),
            ("[]", "JSON object"),
            ("tolerations: []", "not JSON"),
        ] {
            let err = PluginPlacement::parse(raw).unwrap_err();
            assert!(err.contains(says), "{raw}: {err}");
        }
    }
}

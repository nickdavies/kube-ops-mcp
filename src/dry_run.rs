//! Server-side apply dry-runs of a multi-document manifest, reported as a
//! diff against the live object.

use kube::{
    Api, Client,
    api::{DynamicObject, Patch, PatchParams},
    core::GroupVersionKind,
    discovery::{self, Scope},
};
use serde_json::Value;

use crate::substitute::Redactor;

/// Parses a multi-document YAML stream, skipping empty documents.
pub fn parse_manifests(yaml: &str) -> Result<Vec<DynamicObject>, String> {
    let mut objects = Vec::new();
    for (i, doc) in serde_yaml_ng::Deserializer::from_str(yaml).enumerate() {
        let value: Value =
            serde::Deserialize::deserialize(doc).map_err(|e| format!("document {}: {e}", i + 1))?;
        if value.is_null() {
            continue;
        }
        let object: DynamicObject = serde_json::from_value(value)
            .map_err(|e| format!("document {}: not a Kubernetes object: {e}", i + 1))?;
        objects.push(object);
    }
    Ok(objects)
}

fn gvk_of(object: &DynamicObject) -> Result<GroupVersionKind, String> {
    let types = object
        .types
        .as_ref()
        .ok_or("missing apiVersion/kind".to_string())?;
    GroupVersionKind::try_from(types).map_err(|e| e.to_string())
}

/// Core Secrets are refused outright: the dry-run response is the merged
/// object, which would return the live Secret's data.
pub fn is_secret(gvk: &GroupVersionKind) -> bool {
    gvk.group.is_empty() && gvk.kind == "Secret"
}

/// Renders an object for diffing, without the fields that change on every
/// write and say nothing about the manifest, and with secret values
/// redacted.
fn render(object: &DynamicObject, redactor: &Redactor) -> String {
    let mut object = object.clone();
    object.metadata.managed_fields = None;
    object.metadata.resource_version = None;
    object.metadata.generation = None;
    let mut value = match serde_json::to_value(&object) {
        Ok(value) => value,
        Err(e) => return format!("<unrenderable: {e}>"),
    };
    redactor.value(&mut value);
    serde_yaml_ng::to_string(&value).unwrap_or_else(|e| format!("<unrenderable: {e}>"))
}

/// The diff from `live` (absent when the object would be created) to the
/// dry-run result, or `None` when they match.
pub fn diff(
    live: Option<&DynamicObject>,
    applied: &DynamicObject,
    redactor: &Redactor,
) -> Option<String> {
    let before = live.map(|l| render(l, redactor)).unwrap_or_default();
    let after = render(applied, redactor);
    if before == after {
        return None;
    }
    Some(
        similar::TextDiff::from_lines(&before, &after)
            .unified_diff()
            .context_radius(3)
            .header("live", "dry-run")
            .to_string(),
    )
}

pub struct Options {
    pub field_manager: String,
    /// Namespace for namespaced objects that don't set one, like a Flux
    /// Kustomization's `targetNamespace`.
    pub default_namespace: Option<String>,
    /// Applied to rendered objects and to the whole report.
    pub redactor: Redactor,
}

/// Dry-runs every object and returns one report section per object. A
/// failure on one object doesn't stop the rest.
pub async fn run(client: &Client, objects: Vec<DynamicObject>, options: &Options) -> String {
    let mut report = String::new();
    for object in objects {
        let name = object.metadata.name.clone().unwrap_or_default();
        let kind = object
            .types
            .as_ref()
            .map(|t| t.kind.clone())
            .unwrap_or_default();
        let (label, outcome) = match apply_one(client, object, options).await {
            Ok((label, outcome)) => (label, outcome),
            Err((label, error)) => (label, format!("error: {error}")),
        };
        let label = if label.is_empty() {
            format!("{kind}/{name}")
        } else {
            label
        };
        report.push_str(&format!("## {label}\n{outcome}\n\n"));
    }
    // Names in labels and values echoed in API errors aren't rendered
    // objects, so redact the text as well.
    options.redactor.text(&report)
}

type Labelled<T> = Result<T, (String, String)>;

async fn apply_one(
    client: &Client,
    mut object: DynamicObject,
    options: &Options,
) -> Labelled<(String, String)> {
    let gvk = gvk_of(&object).map_err(|e| (String::new(), e))?;
    let name = object
        .metadata
        .name
        .clone()
        .ok_or((String::new(), "missing metadata.name".to_string()))?;
    if is_secret(&gvk) {
        return Err((
            format!("Secret/{name}"),
            "refused: Secrets are not dry-run".to_string(),
        ));
    }

    let (resource, caps) = discovery::pinned_kind(client, &gvk)
        .await
        .map_err(|e| (format!("{}/{name}", gvk.kind), e.to_string()))?;
    let api: Api<DynamicObject> = match caps.scope {
        Scope::Cluster => {
            object.metadata.namespace = None;
            Api::all_with(client.clone(), &resource)
        }
        Scope::Namespaced => {
            let ns = object
                .metadata
                .namespace
                .clone()
                .or_else(|| options.default_namespace.clone())
                .ok_or((
                    format!("{}/{name}", gvk.kind),
                    "namespaced object without metadata.namespace or a default namespace"
                        .to_string(),
                ))?;
            object.metadata.namespace = Some(ns.clone());
            Api::namespaced_with(client.clone(), &ns, &resource)
        }
    };
    let label = match &object.metadata.namespace {
        Some(ns) => format!("{}/{ns}/{name}", gvk.kind),
        None => format!("{}/{name}", gvk.kind),
    };
    let fail = |e: kube::Error| (label.clone(), e.to_string());

    let live = api.get_opt(&name).await.map_err(fail)?;
    let params = PatchParams::apply(&options.field_manager).force().dry_run();
    let applied = api
        .patch(&name, &params, &Patch::Apply(&object))
        .await
        .map_err(fail)?;

    let outcome = match (
        live.is_some(),
        diff(live.as_ref(), &applied, &options.redactor),
    ) {
        (false, Some(d)) => format!("created\n```diff\n{d}```"),
        (true, Some(d)) => format!("configured\n```diff\n{d}```"),
        (_, None) => "unchanged".to_string(),
    };
    Ok((label, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_document_streams_and_skips_empty_documents() {
        let yaml = "---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n---\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: b\n";
        let objects = parse_manifests(yaml).unwrap();
        let names: Vec<_> = objects
            .iter()
            .map(|o| o.metadata.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(gvk_of(&objects[1]).unwrap().group, "apps");
    }

    #[test]
    fn rejects_documents_that_are_not_objects() {
        let err = parse_manifests("apiVersion: v1\nkind: ConfigMap\n---\n- a list\n").unwrap_err();
        assert!(err.starts_with("document 2:"), "{err}");
    }

    #[test]
    fn only_core_secrets_are_refused() {
        let secret = GroupVersionKind::gvk("", "v1", "Secret");
        let other = GroupVersionKind::gvk("example.io", "v1", "Secret");
        assert!(is_secret(&secret));
        assert!(!is_secret(&other));
    }

    #[test]
    fn diff_ignores_write_bookkeeping() {
        let live = parse_manifests(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n  resourceVersion: '1'\ndata:\n  k: v\n",
        )
        .unwrap()
        .remove(0);
        let mut applied = live.clone();
        applied.metadata.resource_version = Some("2".into());
        let none = Redactor::default();
        assert_eq!(diff(Some(&live), &applied, &none), None);

        applied.data["data"]["k"] = "changed".into();
        let d = diff(Some(&live), &applied, &none).unwrap();
        assert!(d.contains("-  k: v") && d.contains("+  k: changed"), "{d}");
    }

    #[test]
    fn diff_redacts_both_sides() {
        let live = parse_manifests(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\ndata:\n  host: a.example.com\n",
        )
        .unwrap()
        .remove(0);
        let mut applied = live.clone();
        let redactor = Redactor::new([("DOMAIN", "example.com")]);
        assert_eq!(diff(Some(&live), &applied, &redactor), None);

        applied.data["data"]["host"] = "b.example.com".into();
        let d = diff(None, &applied, &redactor).unwrap();
        assert!(
            d.contains("b.<redacted:DOMAIN>") && !d.contains("example"),
            "{d}"
        );
    }
}

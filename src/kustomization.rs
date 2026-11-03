//! Flux Kustomization post-build substitution, as kustomize-controller
//! would apply it, so dry-runs see real values instead of `${VAR}`.
//!
//! The variables come only from the live Kustomization's own
//! `spec.postBuild`, so a caller picks a Kustomization, never a Secret.

use std::collections::HashMap;

use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::{
    Api, Client,
    api::{ApiResource, DynamicObject},
    core::GroupVersionKind,
};
use serde::Deserialize;
use serde_json::Value;

use crate::substitute;

const DISABLED_KEY: &str = "kustomize.toolkit.fluxcd.io/substitute";

/// The Kustomization fields substitution depends on.
pub struct Kustomization {
    pub target_namespace: Option<String>,
    pub vars: HashMap<String, String>,
    /// Every (name, value) loaded from a Secret, for redaction. Kept even
    /// when a later source overrides the name: the value is still secret.
    pub secret_vars: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct SubstituteReference {
    kind: String,
    name: String,
    #[serde(default)]
    optional: bool,
}

/// Flux strips newlines from every value.
fn flatten(value: &str) -> String {
    value.replace('\n', "")
}

pub async fn load(client: &Client, namespace: &str, name: &str) -> Result<Kustomization, String> {
    let gvk = GroupVersionKind::gvk("kustomize.toolkit.fluxcd.io", "v1", "Kustomization");
    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), namespace, &ApiResource::from_gvk(&gvk));
    let ks = api
        .get(name)
        .await
        .map_err(|e| format!("Kustomization {namespace}/{name}: {e}"))?;
    let spec = &ks.data["spec"];
    let post_build = &spec["postBuild"];

    let mut vars = HashMap::new();
    let mut secret_vars = Vec::new();
    let references: Vec<SubstituteReference> = match &post_build["substituteFrom"] {
        Value::Null => Vec::new(),
        refs => serde_json::from_value(refs.clone())
            .map_err(|e| format!("spec.postBuild.substituteFrom: {e}"))?,
    };
    for reference in references {
        let missing = |e: &kube::Error| {
            reference.optional && matches!(e, kube::Error::Api(s) if s.code == 404)
        };
        match reference.kind.as_str() {
            "ConfigMap" => {
                match Api::<ConfigMap>::namespaced(client.clone(), namespace)
                    .get(&reference.name)
                    .await
                {
                    Ok(cm) => {
                        for (k, v) in cm.data.unwrap_or_default() {
                            vars.insert(k, flatten(&v));
                        }
                    }
                    Err(e) if missing(&e) => {}
                    Err(e) => return Err(format!("ConfigMap/{}: {e}", reference.name)),
                }
            }
            "Secret" => {
                match Api::<Secret>::namespaced(client.clone(), namespace)
                    .get(&reference.name)
                    .await
                {
                    Ok(secret) => {
                        for (k, v) in secret.data.unwrap_or_default() {
                            let value = flatten(&String::from_utf8_lossy(&v.0));
                            secret_vars.push((k.clone(), value.clone()));
                            vars.insert(k, value);
                        }
                    }
                    Err(e) if missing(&e) => {}
                    Err(e) => return Err(format!("Secret/{}: {e}", reference.name)),
                }
            }
            other => return Err(format!("unsupported substituteFrom kind {other}")),
        }
    }
    if let Some(inline) = post_build["substitute"].as_object() {
        for (k, v) in inline {
            vars.insert(k.clone(), flatten(v.as_str().unwrap_or_default()));
        }
    }
    if let Some(bad) = vars.keys().find(|k| !valid_name(k)) {
        return Err(format!(
            "'{bad}' var name is invalid, must match ^[_[:alpha:]][_[:alpha:][:digit:]]*$"
        ));
    }

    Ok(Kustomization {
        target_namespace: spec["targetNamespace"].as_str().map(String::from),
        vars,
        secret_vars,
    })
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Splits a `---`-separated stream into document texts. Substitution runs on
/// each document's own text, as Flux runs it on each resource's YAML.
pub fn split_documents(yaml: &str) -> Vec<String> {
    let mut documents = vec![String::new()];
    for line in yaml.lines() {
        if line.trim_end() == "---" || line.starts_with("--- ") {
            documents.push(String::new());
        } else {
            let current = documents.last_mut().expect("never empty");
            current.push_str(line);
            current.push('\n');
        }
    }
    documents.retain(|d| !d.trim().is_empty());
    documents
}

fn substitution_disabled(document: &Value) -> bool {
    let metadata = &document["metadata"];
    [&metadata["labels"], &metadata["annotations"]]
        .iter()
        .any(|m| m[DISABLED_KEY].as_str() == Some("disabled"))
}

/// Expands one document unless it opts out with the
/// `kustomize.toolkit.fluxcd.io/substitute: disabled` label or annotation.
pub fn substitute_document(
    document: &str,
    vars: &HashMap<String, String>,
) -> Result<String, String> {
    let parsed: Value =
        serde_yaml_ng::from_str(document).map_err(|e| format!("parsing manifests: {e}"))?;
    if vars.is_empty() || substitution_disabled(&parsed) {
        return Ok(document.to_string());
    }
    substitute::eval(document, vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_separator_lines_only() {
        let docs = split_documents("---\na: 1\n---\n\n--- # c\nb: '---'\n");
        assert_eq!(docs, ["a: 1\n", "b: '---'\n"]);
    }

    #[test]
    fn honours_the_disabled_label_and_annotation() {
        let vars = HashMap::from([("X".to_string(), "v".to_string())]);
        for key in ["labels", "annotations"] {
            let doc = format!(
                "kind: ConfigMap\nmetadata:\n  {key}:\n    kustomize.toolkit.fluxcd.io/substitute: disabled\ndata:\n  s: ${{username}}\n"
            );
            assert_eq!(substitute_document(&doc, &vars).unwrap(), doc);
        }
        let doc = "kind: ConfigMap\ndata:\n  s: ${X}\n";
        assert!(substitute_document(doc, &vars).unwrap().contains("s: v"));
    }

    #[test]
    fn variable_names_follow_flux() {
        assert!(valid_name("SECRET_DOMAIN") && valid_name("_x1"));
        assert!(!valid_name("1x") && !valid_name("a-b") && !valid_name(""));
    }
}

//! The MCP tool surface.

use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{dry_run, kustomization, substitute::Redactor};

#[derive(Clone)]
pub struct KubeOps {
    client: Client,
    tool_router: ToolRouter<Self>,
}

impl KubeOps {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct DryRunApplyRequest {
    /// Multi-document YAML, e.g. the output of `flux build kustomization` or
    /// `kustomize build`.
    manifests: String,
    /// Namespace for namespaced objects that don't set one.
    namespace: Option<String>,
    /// Field manager to apply as. Defaults to `kustomize-controller` so
    /// fields Flux owns can be dropped, exactly as a real reconcile would.
    field_manager: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DryRunKustomizationRequest {
    /// Name of the live Flux Kustomization whose `spec.postBuild` supplies
    /// the variables.
    kustomization: String,
    /// The Kustomization's namespace. Defaults to `flux-system`.
    namespace: Option<String>,
    /// Output of `flux build kustomization <name> -n flux-system --path
    /// <dir> --kustomization-file <ks.yaml> --dry-run`, with `${VAR}` still
    /// literal.
    manifests: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SecretKeysRequest {
    namespace: String,
    name: String,
}

/// Key names only. `stringData` is write-only (the API server folds it into
/// `data`), so `data` holds every key.
fn key_names(secret: &Secret) -> Vec<String> {
    secret.data.iter().flat_map(|d| d.keys().cloned()).collect()
}

#[tool_router]
impl KubeOps {
    #[tool(
        description = "Server-side apply dry-run of Kubernetes manifests against the live \
        cluster, with force-conflicts, as Flux's field manager. Reports per object whether \
        it would be created, configured or unchanged, with a diff from the live object, or \
        the API server's error. Nothing is persisted. Secrets are refused."
    )]
    async fn dry_run_apply(
        &self,
        Parameters(request): Parameters<DryRunApplyRequest>,
    ) -> Result<String, String> {
        let objects = dry_run::parse_manifests(&request.manifests)?;
        if objects.is_empty() {
            return Err("no objects in manifests".into());
        }
        tracing::info!(objects = objects.len(), "dry_run_apply");
        let options = dry_run::Options {
            field_manager: request
                .field_manager
                .unwrap_or_else(|| "kustomize-controller".into()),
            default_namespace: request.namespace,
            redactor: Redactor::default(),
        };
        Ok(dry_run::run(&self.client, objects, &options).await)
    }

    #[tool(
        description = "Dry-run a Flux Kustomization's build exactly as kustomize-controller \
        would apply it: substitutes the live Kustomization's postBuild variables (its \
        substitute and substituteFrom ConfigMaps and Secrets, strict mode), then runs \
        dry_run_apply's server-side dry-run as kustomize-controller. Prefer this over \
        dry_run_apply for anything Flux applies, since fields using ${VAR} then diff \
        against real values.\n\n\
        Values that came from a Secret are shown as <redacted:NAME>, in diffs and errors \
        alike. Only ${VAR} and ${VAR:-default} (also = and :=) are expanded; Flux's other \
        operators (${#VAR}, ${VAR:0:1}, ${VAR^^}, ${VAR/a/b}, ...) are refused, because \
        they could read a Secret back past the redaction."
    )]
    async fn dry_run_kustomization(
        &self,
        Parameters(request): Parameters<DryRunKustomizationRequest>,
    ) -> Result<String, String> {
        let namespace = request.namespace.unwrap_or_else(|| "flux-system".into());
        let ks = kustomization::load(&self.client, &namespace, &request.kustomization).await?;
        let redactor = Redactor::new(
            ks.secret_vars
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );

        let mut objects = Vec::new();
        for document in kustomization::split_documents(&request.manifests) {
            let expanded = kustomization::substitute_document(&document, &ks.vars)
                .and_then(|text| dry_run::parse_manifests(&text))
                // Parse errors can quote the substituted text.
                .map_err(|e| redactor.text(&e))?;
            objects.extend(expanded);
        }
        if objects.is_empty() {
            return Err("no objects in manifests".into());
        }
        tracing::info!(
            %namespace,
            kustomization = %request.kustomization,
            objects = objects.len(),
            "dry_run_kustomization"
        );
        let options = dry_run::Options {
            field_manager: "kustomize-controller".into(),
            default_namespace: ks.target_namespace,
            redactor,
        };
        Ok(dry_run::run(&self.client, objects, &options).await)
    }

    #[tool(
        description = "List the key names of a Secret, never the values. Use it to check \
        that an ExternalSecret or other generator produced the keys an app expects."
    )]
    async fn secret_keys(
        &self,
        Parameters(request): Parameters<SecretKeysRequest>,
    ) -> Result<String, String> {
        tracing::info!(namespace = %request.namespace, name = %request.name, "secret_keys");
        let secret = Api::<Secret>::namespaced(self.client.clone(), &request.namespace)
            .get(&request.name)
            .await
            .map_err(|e| e.to_string())?;
        Ok(key_names(&secret).join("\n"))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KubeOps {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::ByteString;
    use std::collections::BTreeMap;

    #[test]
    fn key_names_are_sorted_and_never_carry_values() {
        let secret = Secret {
            data: Some(BTreeMap::from([
                ("b".to_string(), ByteString(b"hunter2".to_vec())),
                ("a".to_string(), ByteString(b"x".to_vec())),
            ])),
            ..Default::default()
        };
        let keys = key_names(&secret);
        assert_eq!(keys, ["a", "b"]);
        assert!(!keys.join("").contains("hunter2"));
    }
}

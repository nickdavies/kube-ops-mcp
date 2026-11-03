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

use crate::dry_run;

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
    /// Multi-document YAML. For a Flux Kustomization, use the output of
    /// `flux build kustomization <name> -n flux-system --path <dir>
    /// --kustomization-file <ks.yaml> --dry-run`.
    manifests: String,
    /// Namespace for namespaced objects that don't set one.
    namespace: Option<String>,
    /// Field manager to apply as. Defaults to `kustomize-controller` so
    /// fields Flux owns can be dropped, exactly as a real reconcile would.
    field_manager: Option<String>,
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
        the API server's error. Nothing is persisted. Secrets are refused.\n\n\
        For a Flux Kustomization, pass the output of `flux build kustomization <name> \
        -n flux-system --path <dir> --kustomization-file <ks.yaml> --dry-run`. Plain \
        `kustomize build` output lacks the labels Flux adds, so every object would show \
        them removed. Without `--dry-run`, `flux build` reads the substitution Secrets \
        and fails for callers that can't. `--dry-run` skips Flux's ${VAR} substitution, \
        so any field that uses a variable shows as changed to the literal `${VAR}`: treat \
        those lines as noise, not as real changes."
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

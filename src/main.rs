//! kube-ops-mcp: an MCP server for the cluster operations an agent needs but
//! Kubernetes RBAC can't grant safely.
//!
//! - `dry_run_apply` needs create/patch on every kind (RBAC has no dry-run
//!   verb), and a dry-run response would echo back Secret data the caller can't
//!   otherwise read. This server holds those verbs and refuses Secrets.
//! - `secret_keys` lists a Secret's key names without the values; RBAC can
//!   only grant `get`, which returns the values.
//!
//! Callers authenticate with a projected ServiceAccount token whose audience
//! is `TOKEN_AUDIENCE`; see [`auth`].

mod auth;
mod dry_run;
mod server;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Router, middleware, routing::get};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::signal::unix::SignalKind;
use tokio_util::sync::CancellationToken;

/// Runtime configuration, read from the environment.
struct Config {
    /// Socket to listen on, e.g. `0.0.0.0:8080`.
    listen_addr: String,
    /// Audience a caller's token must be issued for.
    token_audience: String,
    /// Usernames allowed to call tools, e.g.
    /// `system:serviceaccount:workspaces:ws-homelab`.
    allowed_users: Vec<String>,
    /// `Host` header values the MCP endpoint accepts (DNS-rebinding guard).
    allowed_hosts: Vec<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let list = |name: &str| -> Result<Vec<String>> {
            let raw = std::env::var(name).with_context(|| format!("{name} must be set"))?;
            let items: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            anyhow::ensure!(!items.is_empty(), "{name} must not be empty");
            Ok(items)
        };
        Ok(Self {
            listen_addr: std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            token_audience: std::env::var("TOKEN_AUDIENCE")
                .unwrap_or_else(|_| "kube-ops-mcp".into()),
            allowed_users: list("ALLOWED_USERS")?,
            allowed_hosts: list("ALLOWED_HOSTS")?,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env()?;
    let client = kube::Client::try_default()
        .await
        .context("building kube client")?;

    let ct = CancellationToken::new();
    let mcp_client = client.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(server::KubeOps::new(mcp_client.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(config.allowed_hosts)
            .with_cancellation_token(ct.child_token()),
    );

    let authenticator = Arc::new(auth::Authenticator::new(
        client,
        config.token_audience,
        config.allowed_users,
    ));
    let app = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(authenticator, auth::require))
        // Probes sit outside the auth layer.
        .route("/healthz", get(|| async { "ok" }));

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    tracing::info!(addr = %config.listen_addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut term = tokio::signal::unix::signal(SignalKind::terminate())
                .expect("installing SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            ct.cancel();
        })
        .await?;
    Ok(())
}

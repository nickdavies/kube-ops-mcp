//! Caller authentication: a bearer ServiceAccount token, checked with a
//! TokenReview against a dedicated audience, from an allowlisted user.
//!
//! The dedicated audience matters: a token minted for this server can't be
//! replayed against the API server, and the API server's own tokens are
//! rejected here.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::{Api, api::PostParams};

pub struct Authenticator {
    reviews: Api<TokenReview>,
    audience: String,
    allowed_users: Vec<String>,
}

impl Authenticator {
    pub fn new(client: kube::Client, audience: String, allowed_users: Vec<String>) -> Self {
        Self {
            reviews: Api::all(client),
            audience,
            allowed_users,
        }
    }

    /// Returns the authenticated username, or the status to reject with.
    async fn authenticate(&self, token: &str) -> Result<String, StatusCode> {
        let review = TokenReview {
            spec: TokenReviewSpec {
                token: Some(token.to_string()),
                audiences: Some(vec![self.audience.clone()]),
            },
            ..Default::default()
        };
        let status = self
            .reviews
            .create(&PostParams::default(), &review)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "TokenReview failed");
                StatusCode::SERVICE_UNAVAILABLE
            })?
            .status
            .unwrap_or_default();

        let audience_ok = status
            .audiences
            .as_ref()
            .is_some_and(|a| a.contains(&self.audience));
        if status.authenticated != Some(true) || !audience_ok {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let username = status.user.and_then(|u| u.username).unwrap_or_default();
        if !self.allowed_users.contains(&username) {
            tracing::warn!(%username, "authenticated but not allowed");
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(username)
    }
}

pub async fn require(
    State(auth): State<Arc<Authenticator>>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match auth.authenticate(token).await {
        Ok(username) => {
            tracing::info!(%username, method = %request.method(), "request");
            next.run(request).await
        }
        Err(status) => status.into_response(),
    }
}

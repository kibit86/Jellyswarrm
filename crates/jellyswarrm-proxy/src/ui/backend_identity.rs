//! Who the proxy is, on one backend, when it opens that backend's own web client.
//!
//! The administrator maps, for each server, the backend account to use (that is
//! all a mapping is). Opening a backend's dashboard signs in with that account,
//! so there is no second login and no dependence on any app having signed in.
//! The identity is cached briefly: the pass-through asks for it on every request.
use std::{sync::Arc, time::Duration};

use axum::http::StatusCode;
use moka::future::Cache;
use tracing::{debug, warn};

use crate::{
    auth::mapping_authentication::MappingAuthenticationError, http::state::ManagementState,
    servers::storage::Server,
};

const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// The backend session the pass-through acts with.
#[derive(Debug, Clone)]
pub(crate) struct PanelIdentity {
    /// Backend access token.
    pub(crate) token: String,
    /// Backend user ID of the mapped account.
    pub(crate) user_id: String,
    /// Backend's own server ID (what its web client stores credentials under).
    pub(crate) backend_server_id: String,
    pub(crate) is_administrator: bool,
}

/// Why no identity could be obtained, as the HTTP status the pass-through answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityError {
    /// The user has no mapped account on this server.
    NotConnected,
    /// The mapped account is not accepted by the backend (or the backend is down).
    BackendRefused,
    /// Local storage or credential handling failed.
    Internal,
}

impl From<IdentityError> for StatusCode {
    fn from(error: IdentityError) -> Self {
        match error {
            IdentityError::NotConnected => StatusCode::FORBIDDEN,
            IdentityError::BackendRefused => StatusCode::BAD_GATEWAY,
            IdentityError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Identities by (proxy user, server). Concurrent requests for the same key share
/// one login: a page load asks for dozens of resources at once.
pub(crate) struct PanelIdentities {
    cache: Cache<(String, i64), Arc<PanelIdentity>>,
}

impl Default for PanelIdentities {
    fn default() -> Self {
        Self::new()
    }
}

impl PanelIdentities {
    pub(crate) fn new() -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(CACHE_TTL)
                .max_capacity(256)
                .build(),
        }
    }

    /// Forget what is known about a user's account on a server (its mapping changed).
    pub(crate) async fn forget(&self, user_id: &str, server: &Server) {
        self.cache
            .invalidate(&(user_id.to_owned(), server.id.as_i64()))
            .await;
    }

    pub(crate) async fn get(
        &self,
        state: &ManagementState,
        user_id: &str,
        server: &Server,
    ) -> Result<Arc<PanelIdentity>, IdentityError> {
        self.cache
            .try_get_with(
                (user_id.to_owned(), server.id.as_i64()),
                Self::login(state, user_id, server),
            )
            .await
            .map_err(|error| *error)
    }

    async fn login(
        state: &ManagementState,
        user_id: &str,
        server: &Server,
    ) -> Result<Arc<PanelIdentity>, IdentityError> {
        let storage = |error: sqlx::Error| {
            warn!(%error, "Could not read the account mapping");
            IdentityError::Internal
        };
        let mapping = state
            .user_authorization
            .get_server_mapping(user_id, server)
            .await
            .map_err(storage)?
            .ok_or(IdentityError::NotConnected)?;
        let user = state
            .user_authorization
            .get_user_by_id(user_id)
            .await
            .map_err(storage)?
            .ok_or(IdentityError::NotConnected)?;
        let client_info = jellyfin_api::ClientInfo {
            device: "Jellyswarrm panel".to_string(),
            // One device per user and server: the backend reuses its session
            // instead of piling up a new one at every login.
            device_id: format!("jellyswarrm-panel-{}-{}", server.id, user.id),
            ..crate::config::CLIENT_INFO.clone()
        };
        let login = state
            .mapping_authenticator
            .authenticate_login(&user, server, &mapping, None, client_info)
            .await
            .map_err(|error| {
                warn!(server_id = %server.id, %error, "The mapped account could not sign in");
                match error {
                    MappingAuthenticationError::Storage(_)
                    | MappingAuthenticationError::Credential(_) => IdentityError::Internal,
                    _ => IdentityError::BackendRefused,
                }
            })?;
        let identity = PanelIdentity {
            token: login.response.access_token,
            user_id: login.response.user.id,
            backend_server_id: login.response.server_id,
            is_administrator: login.response.user.policy.is_administrator,
        };
        state
            .admin_rights
            .record(&identity.token, identity.is_administrator)
            .await;
        debug!(
            server_id = %server.id,
            is_administrator = identity.is_administrator,
            "Signed in to the backend for the panel"
        );
        Ok(Arc::new(identity))
    }
}

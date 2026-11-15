//! Media capabilities shared by catalog, playback, routing, and generic forwarding.
//! Every handle is reference counted: workflows borrow `&MediaServices`, and
//! bounded source tasks clone it. Accounts, administration, SyncPlay, and Quick
//! Connect keep their own state and are not reachable from here.
use crate::{
    application::{
        catalog::snapshots::CatalogSnapshots,
        error::ApplicationError,
        request::{response, UpstreamRequestBuilder},
    },
    auth::UserAuthorizationService,
    client_sessions::ClientSessionService,
    config::SettingsView,
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    playback::PlaybackRegistry,
    servers::{Server, ServerStorageService},
    translation::{
        response_processor::{ResponseProcessingContext, ResponseProcessingProfile},
        ProxyProcessors,
    },
};
use std::sync::Arc;

use super::request::selection::UrlSelection;

#[derive(Clone)]
pub(crate) struct MediaServices {
    pub(crate) reqwest_client: reqwest::Client,
    pub(crate) streaming_reqwest_client: reqwest::Client,
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) media_storage: Arc<MediaStorageService>,
    pub(crate) virtual_library_service: Arc<VirtualLibraryService>,
    pub(crate) playback_registry: Arc<PlaybackRegistry>,
    pub(crate) config: SettingsView,
    pub(crate) url_selection: UrlSelection,
    pub(crate) translation: Arc<ProxyProcessors>,
    pub(crate) client_sessions: Arc<ClientSessionService>,
    pub(crate) catalog_snapshots: Arc<CatalogSnapshots>,
    pub(crate) admin_rights: Arc<super::admin_rights::AdminRights>,
    /// Fixed at startup, like the router mount it mirrors.
    pub(crate) url_prefix: Option<String>,
}

impl MediaServices {
    pub(crate) fn url_prefix(&self) -> Option<&str> {
        self.url_prefix.as_deref()
    }

    pub(crate) fn remove_prefix<'a>(&self, path: &'a str) -> &'a str {
        crate::protocol::url::strip_url_prefix(path, self.url_prefix())
    }

    pub(crate) fn request_builder(&self) -> UpstreamRequestBuilder<'_> {
        UpstreamRequestBuilder::new(&self.translation.url_processor, self.url_prefix())
    }

    pub(crate) async fn deduplicate_media_enabled(&self) -> bool {
        self.config.read().await.deduplicate_media
    }

    pub(crate) async fn merge_libraries_enabled(&self) -> bool {
        self.config.read().await.merge_libraries
    }

    pub(crate) async fn process_response_json(
        &self,
        payload: &mut serde_json::Value,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<bool, ApplicationError> {
        let context = ResponseProcessingContext {
            server: server.clone(),
            proxy_server_id: self.config.read().await.server_id.clone(),
            proxy_api_key: token.map(str::to_owned),
            profile,
        };
        let modified = self
            .translation
            .process_response_json(payload, &context)
            .await?;
        // Tell the web client which items its viewer may manage.
        // Only from what is already known: a response never waits for a lookup.
        let annotated = match token {
            Some(token) if profile != ResponseProcessingProfile::Disabled => {
                match self
                    .admin_rights
                    .known_administrator_for_token(&self.user_authorization, token, server)
                    .await
                {
                    Some(can_manage) => {
                        super::admin_rights::annotate_can_manage(payload, can_manage)
                    }
                    None => false,
                }
            }
            _ => false,
        };
        Ok(modified || annotated)
    }

    pub(crate) async fn execute_processed_json(
        &self,
        request: reqwest::Request,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<serde_json::Value, ApplicationError> {
        let mut payload = response::execute_json_request(&self.reqwest_client, request).await?;
        self.process_response_json(&mut payload, server, profile, token)
            .await?;
        Ok(payload)
    }
}

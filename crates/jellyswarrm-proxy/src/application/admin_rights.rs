//! Administrator rights of a proxy user on each backend, and the request
//! classes that need them.
//!
//! The proxy never reports a backend administrator as such to its clients by
//! default. This module answers "is this user an administrator on that server?"
//! (cached, looked up with the user's own backend session) and decides which
//! client requests are item management (allowed only on servers where the user
//! is an administrator) or global server administration (never served by the
//! merged API: it has no single target, so it goes through the per-server
//! pass-through instead).
use std::{collections::HashSet, time::Duration};

use hyper::Method;
use moka::future::Cache;
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    auth::{service::AuthorizationSession, UserAuthorizationService},
    domain::ids::ServerId,
    servers::storage::Server,
};

/// Field added to every item of a response: whether the viewer may manage that
/// item (administrator on the backend that owns it). The patched web client
/// reads it; other clients ignore unknown fields.
pub(crate) const CAN_MANAGE_FIELD: &str = "JellyswarrmCanManage";

/// Add [`CAN_MANAGE_FIELD`] to every item-like object (has `Id` and `Type`).
/// Returns whether anything was added.
pub(crate) fn annotate_can_manage(value: &mut serde_json::Value, can_manage: bool) -> bool {
    match value {
        serde_json::Value::Array(items) => items
            .iter_mut()
            .fold(false, |added, item| annotate_can_manage(item, can_manage) | added),
        serde_json::Value::Object(map) => {
            let is_item = map.get("Id").is_some_and(serde_json::Value::is_string)
                && map.get("Type").is_some_and(serde_json::Value::is_string);
            let mut added = false;
            if is_item {
                map.insert(CAN_MANAGE_FIELD.into(), serde_json::Value::Bool(can_manage));
                added = true;
            }
            map.iter_mut()
                .filter(|(key, _)| key.as_str() != CAN_MANAGE_FIELD)
                .fold(added, |added, (_, child)| annotate_can_manage(child, can_manage) | added)
        }
        _ => false,
    }
}

const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How a client request relates to backend administration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminRequestClass {
    /// Not an administrative request.
    Regular,
    /// Admin-only item operations (refresh, images, identify, subtitles...):
    /// routed only to backends where the caller is an administrator.
    ItemManagement,
    /// `POST|DELETE /Items/{id}`: metadata edit or delete, but also how a user
    /// deletes their own playlist or collection. Prefer a backend where the
    /// caller is an administrator; otherwise the backend itself decides.
    ItemWrite,
    /// Server-wide administration (configuration, libraries, tasks, users...).
    /// The merged API cannot pick one server for it, so it is refused.
    GlobalAdministration,
}

/// Classify a request by method and path (URL prefix already removed).
pub(crate) fn classify_request(method: &Method, path: &str) -> AdminRequestClass {
    let decoded = percent_decode_str(path).decode_utf8_lossy().to_ascii_lowercase();
    let segments: Vec<&str> = decoded.split('/').filter(|part| !part.is_empty()).collect();
    let read_only = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    let Some(first) = segments.first().copied() else {
        return AdminRequestClass::Regular;
    };
    let second = segments.get(1).copied();
    let third = segments.get(2).copied();

    match first {
        "items" => match (second, third) {
            (Some("remotesearch"), _) => AdminRequestClass::ItemManagement,
            // Items/{id}: metadata update or delete.
            (Some(_), None) if !read_only => AdminRequestClass::ItemWrite,
            (Some(_), Some("refresh" | "metadataeditor" | "contenttype" | "remotesearch")) => {
                AdminRequestClass::ItemManagement
            }
            (Some(_), Some("images" | "externalidinfos")) if !read_only => {
                AdminRequestClass::ItemManagement
            }
            (Some("metadata"), _) if !read_only => AdminRequestClass::ItemManagement,
            _ => AdminRequestClass::Regular,
        },
        // Subtitle and lyric uploads/deletions, alternate sources, merging.
        "videos" => match third {
            Some("subtitles" | "alternatesources") if !read_only => {
                AdminRequestClass::ItemManagement
            }
            None if second == Some("mergeversions") && !read_only => {
                AdminRequestClass::ItemManagement
            }
            _ => AdminRequestClass::Regular,
        },
        "audio" => match third {
            Some("lyrics" | "remotesearch") if !read_only => AdminRequestClass::ItemManagement,
            _ => AdminRequestClass::Regular,
        },
        "system" => match second {
            Some("configuration" | "restart" | "shutdown" | "logs") => {
                AdminRequestClass::GlobalAdministration
            }
            _ => AdminRequestClass::Regular,
        },
        "library" => match second {
            Some("virtualfolders" | "refresh" | "series" | "movies" | "media") => {
                AdminRequestClass::GlobalAdministration
            }
            _ => AdminRequestClass::Regular,
        },
        "scheduledtasks" | "plugins" | "packages" | "repositories" | "startup"
        | "environment" | "devices" => AdminRequestClass::GlobalAdministration,
        "notifications" if second == Some("admin") => AdminRequestClass::GlobalAdministration,
        "users" => match (second, third) {
            (Some("new"), _) if !read_only => AdminRequestClass::GlobalAdministration,
            (Some(_), Some("policy")) if !read_only => AdminRequestClass::GlobalAdministration,
            (Some(_), None) if *method == Method::DELETE => {
                AdminRequestClass::GlobalAdministration
            }
            _ => AdminRequestClass::Regular,
        },
        _ => AdminRequestClass::Regular,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BackendUser {
    policy: Option<BackendPolicy>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BackendPolicy {
    #[serde(default)]
    is_administrator: bool,
}

/// Per-session administrator flag, cached for a short time.
///
/// A session is one device login of a proxy user on one backend, so the key is
/// that session's backend token. The cache is filled from the backend's login
/// response, or by a `GET /Users/{id}` lookup when a flag is needed and unknown.
/// Lookup failures are treated as "not an administrator" and are not cached:
/// the next request tries again.
pub(crate) struct AdminRights {
    cache: Cache<String, bool>,
}

impl Default for AdminRights {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminRights {
    pub(crate) fn new() -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(CACHE_TTL)
                .max_capacity(4096)
                .build(),
        }
    }

    /// Remember the flag a backend reported for a freshly issued backend token.
    pub(crate) async fn record(&self, backend_token: &str, is_administrator: bool) {
        self.cache
            .insert(backend_token.to_owned(), is_administrator)
            .await;
    }

    /// Whether the session's backend user is an administrator on `server`.
    pub(crate) async fn is_administrator(
        &self,
        client: &reqwest::Client,
        session: &AuthorizationSession,
        server: &Server,
    ) -> bool {
        if let Some(known) = self.cache.get(&session.jellyfin_token).await {
            return known;
        }
        match Self::lookup(client, session, server).await {
            Ok(flag) => {
                self.cache
                    .insert(session.jellyfin_token.clone(), flag)
                    .await;
                flag
            }
            Err(error) => {
                warn!(server_id = %server.id, %error, "Could not read administrator flag from backend");
                false
            }
        }
    }

    /// What is already known about the proxy user behind `token` (proxy API
    /// key or virtual key) on `server`, without any network call: `Some(true)`
    /// when a session there is a known administrator, `Some(false)` when all
    /// known sessions there are not, `None` when nothing is known yet.
    pub(crate) async fn known_administrator_for_token(
        &self,
        users: &UserAuthorizationService,
        token: &str,
        server: &Server,
    ) -> Option<bool> {
        let user = users.get_user_by_token(token).await.ok()??;
        let sessions = users.get_user_sessions(&user.id, None).await.ok()?;
        let mut known = None;
        for (session, _) in sessions.iter().filter(|(_, s)| s.id == server.id) {
            match self.cache.get(&session.jellyfin_token).await {
                Some(true) => return Some(true),
                Some(false) => known = Some(false),
                None => {}
            }
        }
        known
    }

    /// Servers of `sessions` where the user is an administrator.
    pub(crate) async fn administrator_servers(
        &self,
        client: &reqwest::Client,
        sessions: &[(AuthorizationSession, Server)],
    ) -> HashSet<ServerId> {
        let checks = sessions.iter().map(|(session, server)| async move {
            self.is_administrator(client, session, server)
                .await
                .then_some(server.id)
        });
        futures_util::future::join_all(checks)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    async fn lookup(
        client: &reqwest::Client,
        session: &AuthorizationSession,
        server: &Server,
    ) -> Result<bool, anyhow::Error> {
        let base = server.url.as_str().trim_end_matches('/');
        let url = format!("{base}/Users/{}", session.original_user_id);
        let response = client
            .get(&url)
            // `Authorization`: Jellyfin 12 no longer reads X-Emby-Authorization.
            .header(
                reqwest::header::AUTHORIZATION,
                format!(
                    "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
                    env!("CARGO_PKG_VERSION"),
                    session.jellyfin_token
                ),
            )
            .timeout(LOOKUP_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        let user: BackendUser = response.json().await?;
        let flag = user.policy.is_some_and(|policy| policy.is_administrator);
        debug!(server_id = %server.id, is_administrator = flag, "Read administrator flag");
        Ok(flag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AdminRequestClass::*;

    #[test]
    fn annotation_marks_items_at_any_depth_and_only_items() {
        let mut page = serde_json::json!({
            "Items": [
                {"Id": "a", "Type": "Movie", "People": [{"Id": "p", "Name": "x"}]},
                {"Id": "b", "Type": "Series"}
            ],
            "TotalRecordCount": 2
        });
        assert!(annotate_can_manage(&mut page, true));
        assert_eq!(page["Items"][0][CAN_MANAGE_FIELD], true);
        assert_eq!(page["Items"][1][CAN_MANAGE_FIELD], true);
        // A person has an Id but no Type: not an item.
        assert!(page["Items"][0]["People"][0].get(CAN_MANAGE_FIELD).is_none());
        assert!(page.get(CAN_MANAGE_FIELD).is_none());

        let mut none = serde_json::json!({"Name": "no items here"});
        assert!(!annotate_can_manage(&mut none, false));
        let mut item = serde_json::json!({"Id": "a", "Type": "Movie"});
        annotate_can_manage(&mut item, false);
        assert_eq!(item[CAN_MANAGE_FIELD], false);
    }

    fn class(method: Method, path: &str) -> AdminRequestClass {
        classify_request(&method, path)
    }

    #[test]
    fn item_edits_and_deletes_are_management() {
        assert_eq!(class(Method::POST, "/Items/abc"), ItemWrite);
        assert_eq!(class(Method::DELETE, "/Items/abc"), ItemWrite);
        assert_eq!(class(Method::POST, "/items/abc/Refresh"), ItemManagement);
        assert_eq!(class(Method::POST, "/Items/abc/Images/Primary"), ItemManagement);
        assert_eq!(class(Method::DELETE, "/Items/abc/Images/Backdrop/0"), ItemManagement);
        assert_eq!(class(Method::GET, "/Items/abc/MetadataEditor"), ItemManagement);
        assert_eq!(class(Method::POST, "/Items/RemoteSearch/Apply/abc"), ItemManagement);
        assert_eq!(class(Method::POST, "/Items/RemoteSearch/Movie"), ItemManagement);
        assert_eq!(class(Method::POST, "/Videos/abc/Subtitles/1"), ItemManagement);
        assert_eq!(class(Method::POST, "/Audio/abc/Lyrics"), ItemManagement);
        assert_eq!(class(Method::POST, "/Videos/MergeVersions"), ItemManagement);
    }

    #[test]
    fn reading_and_playback_are_regular() {
        assert_eq!(class(Method::GET, "/Items/abc"), Regular);
        assert_eq!(class(Method::GET, "/Items/abc/Images/Primary"), Regular);
        assert_eq!(class(Method::POST, "/Items/abc/PlaybackInfo"), Regular);
        assert_eq!(class(Method::GET, "/Items/abc/Download"), Regular);
        assert_eq!(class(Method::GET, "/Videos/abc/Subtitles/1/Stream.srt"), Regular);
        assert_eq!(class(Method::POST, "/Sessions/Playing"), Regular);
        assert_eq!(class(Method::GET, "/System/Info"), Regular);
        assert_eq!(class(Method::GET, "/"), Regular);
    }

    #[test]
    fn server_wide_administration_is_global() {
        for path in [
            "/System/Configuration",
            "/System/Configuration/encoding",
            "/System/Restart",
            "/System/Logs",
            "/Library/VirtualFolders",
            "/Library/Refresh",
            "/ScheduledTasks",
            "/Plugins",
            "/Packages/Installed/x",
            "/Repositories",
            "/Startup/Configuration",
            "/Environment/DirectoryContents",
            "/Devices",
            "/Notifications/Admin",
        ] {
            assert_eq!(class(Method::GET, path), GlobalAdministration, "{path}");
        }
        assert_eq!(class(Method::POST, "/Users/New"), GlobalAdministration);
        assert_eq!(class(Method::POST, "/Users/abc/Policy"), GlobalAdministration);
        assert_eq!(class(Method::DELETE, "/Users/abc"), GlobalAdministration);
        assert_eq!(class(Method::GET, "/Users/abc"), Regular);
    }

    #[test]
    fn encoded_and_mixed_case_paths_do_not_bypass() {
        assert_eq!(class(Method::GET, "/sYsTem/%43onfiguration"), GlobalAdministration);
        assert_eq!(class(Method::POST, "/%49tems/abc"), ItemWrite);
        assert_eq!(class(Method::POST, "//Items//abc/"), ItemWrite);
    }
}

//! Open one backend Jellyfin server through the proxy, unchanged.
//!
//! `/<ui>/backend/{server_id}/...` forwards everything to that backend, so the
//! backend's own web client and dashboard work through the proxy's address.
//! Nothing is translated: IDs, tokens and responses are the backend's. The
//! user signs in on the backend's own login page, and the backend itself
//! decides what they may do.
//!
//! The proxy only decides who may open the door: a signed-in proxy user whose
//! linked account on that server is an administrator there. The proxy's own
//! session cookie is never sent to the backend and the backend's cookies never
//! reach the browser.
use axum::{
    body::Body,
    extract::{
        ws::{
            rejection::WebSocketUpgradeRejection, CloseFrame, Message as ClientMessage, WebSocket,
            WebSocketUpgrade,
        },
        Path, Request, State,
    },
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame as UpstreamCloseFrame},
    Message as UpstreamMessage,
};
use tracing::{debug, warn};

use crate::{
    domain::ids::ServerId,
    http::state::ManagementState,
    servers::storage::Server,
    ui::{auth::AuthenticatedUser, backend_identity::PanelIdentity},
    upstream::headers::is_hop_by_hop_header,
};

/// Request headers that must not travel to the backend: the proxy's session
/// cookie, the browser's own `Host`, and framing the HTTP client sets itself.
fn is_dropped_request_header(name: &HeaderName) -> bool {
    is_hop_by_hop_header(name)
        || matches!(
            name.as_str(),
            "host" | "cookie" | "content-length" | "sec-websocket-key" | "sec-websocket-version"
                | "sec-websocket-extensions" | "sec-websocket-accept"
        )
}

/// Response headers that must not reach the browser: the backend's cookies
/// would collide with the proxy's, and framing is recomputed.
fn is_dropped_response_header(name: &HeaderName) -> bool {
    is_hop_by_hop_header(name) || matches!(name.as_str(), "set-cookie" | "set-cookie2")
}

/// The path under which this backend is served, e.g. `ui/backend/3`.
fn mount(ui_route: &str, server_id: ServerId) -> String {
    format!("/{ui_route}/backend/{server_id}")
}

/// Point a backend redirect back into the pass-through mount.
fn rewrite_location(location: &str, mount: &str, backend_base: &str) -> String {
    let base = backend_base.trim_end_matches('/');
    if let Some(rest) = location.strip_prefix(base) {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') {
            return format!("{mount}{rest}");
        }
    }
    if location.starts_with('/') && !location.starts_with("//") {
        return format!("{mount}{location}");
    }
    location.to_owned()
}

/// `ws://` / `wss://` form of the backend URL for one forwarded path.
fn upstream_url(base: &str, rest: &str, query: Option<&str>, websocket: bool) -> String {
    let base = base.trim_end_matches('/');
    let base = if websocket {
        if let Some(stripped) = base.strip_prefix("https://") {
            format!("wss://{stripped}")
        } else if let Some(stripped) = base.strip_prefix("http://") {
            format!("ws://{stripped}")
        } else {
            base.to_owned()
        }
    } else {
        base.to_owned()
    };
    match query {
        Some(query) if !query.is_empty() => format!("{base}/{rest}?{query}"),
        _ => format!("{base}/{rest}"),
    }
}

/// The page that boots the backend's web client: the only response rewritten.
fn is_web_entry(method: &Method, rest: &str) -> bool {
    if *method != Method::GET {
        return false;
    }
    let path = rest.trim_end_matches('/');
    path == "web" || path == "web/index.html"
}

/// Headers that would make the backend answer the entry page with something other
/// than a plain, complete HTML body that can be edited.
fn is_entry_negotiation_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "accept-encoding" | "if-none-match" | "if-modified-since" | "range" | "if-range"
    )
}

const PANEL_SCRIPT: &str = r#"(function () {
  var PREFIX = %PREFIX%, MOUNT = %MOUNT%, SEED = %SEED%;
  try {
    var real = window.localStorage;
    var scoped = {
      getItem: function (k) { return real.getItem(PREFIX + k); },
      setItem: function (k, v) { real.setItem(PREFIX + k, v); },
      removeItem: function (k) { real.removeItem(PREFIX + k); },
      clear: function () {
        for (var i = real.length - 1; i >= 0; i--) {
          var k = real.key(i);
          if (k && k.indexOf(PREFIX) === 0) { real.removeItem(k); }
        }
      },
      key: function (n) {
        var c = 0;
        for (var i = 0; i < real.length; i++) {
          var k = real.key(i);
          if (k && k.indexOf(PREFIX) === 0) { if (c === n) { return k.slice(PREFIX.length); } c++; }
        }
        return null;
      }
    };
    Object.defineProperty(scoped, 'length', { get: function () {
      var c = 0;
      for (var i = 0; i < real.length; i++) {
        var k = real.key(i);
        if (k && k.indexOf(PREFIX) === 0) { c++; }
      }
      return c;
    } });
    Object.defineProperty(window, 'localStorage', { configurable: true, get: function () { return scoped; } });
    if (SEED) {
      var entry = Object.assign({}, SEED, { ManualAddress: location.origin + MOUNT, DateLastAccessed: Date.now() });
      var raw = scoped.getItem('jellyfin_credentials');
      var creds = raw ? JSON.parse(raw) : {};
      var known = creds.Servers || [];
      var previous = known.filter(function (s) { return s.Id === entry.Id; })[0] || {};
      creds.Servers = [Object.assign({}, previous, entry)].concat(known.filter(function (s) { return s.Id !== entry.Id; }));
      scoped.setItem('jellyfin_credentials', JSON.stringify(creds));
    }
  } catch (e) {
    if (window.console) { console.warn('Jellyswarrm: storage scope not applied', e); }
  }
})();"#;

/// Inline JSON must not be able to close the surrounding `<script>`.
fn json_for_script<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .expect("plain data serializes")
        .replace('<', "\\u003c")
}

/// What the page must do before the backend's web client starts: keep its storage
/// apart from the proxy's own client (and from other backends), and hold the
/// signed-in session for this backend.
fn panel_script(server: &Server, mount: &str, identity: &PanelIdentity) -> String {
    let seed = serde_json::json!({
        "Id": identity.backend_server_id,
        "Name": server.name,
        "LastConnectionMode": 2,
        "UserId": identity.user_id,
        "AccessToken": identity.token,
    });
    PANEL_SCRIPT
        .replace("%PREFIX%", &json_for_script(&format!("jsw-backend-{}:", server.id)))
        .replace("%MOUNT%", &json_for_script(&mount))
        .replace("%SEED%", &json_for_script(&seed))
}

/// Put `script` first in the page's `<head>`, so it runs before anything else.
fn inject_script(html: &str, script: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<head")?;
    let close = open + lower[open..].find('>')? + 1;
    let mut out = String::with_capacity(html.len() + script.len() + 20);
    out.push_str(&html[..close]);
    out.push_str("<script>");
    out.push_str(script);
    out.push_str("</script>");
    out.push_str(&html[close..]);
    Some(out)
}

/// A path that could climb out of the backend's base path.
fn has_dot_segments(rest: &str) -> bool {
    let decoded = percent_encoding::percent_decode_str(rest).decode_utf8_lossy();
    decoded.split(['/', '\\']).any(|part| part == "..")
}

/// Let the user in only if the account they mapped on `server_id` is an
/// administrator there. Returns the server and the signed-in backend identity.
async fn authorize(
    state: &ManagementState,
    user_id: &str,
    server_id: ServerId,
) -> Result<(Server, std::sync::Arc<PanelIdentity>), StatusCode> {
    let server = state
        .server_storage
        .get_server_by_id(server_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let identity = state
        .panel_identities
        .get(state, user_id, &server)
        .await
        .map_err(StatusCode::from)?;
    if !identity.is_administrator {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok((server, identity))
}

/// `/<ui>/backend/{id}` without a path: open the backend's web client.
pub(crate) async fn root(
    State(state): State<ManagementState>,
    Path(server_id): Path<i64>,
) -> Redirect {
    Redirect::temporary(&format!(
        "{}/web/",
        mount(&state.paths.ui_route(), ServerId::new(server_id))
    ))
}

/// `/<ui>/backend/{id}/{*path}`: forward to the backend.
#[axum::debug_handler(state = ManagementState)]
pub(crate) async fn forward(
    State(state): State<ManagementState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path((server_id, rest)): Path<(i64, String)>,
    // Not an upgrade request: an ordinary HTTP call, forwarded below.
    websocket: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: Request,
) -> Response {
    let server_id = ServerId::new(server_id);
    if has_dot_segments(&rest) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let (server, identity) = match authorize(&state, &user.id, server_id).await {
        Ok(authorized) => authorized,
        Err(status) => return status.into_response(),
    };
    let query = request.uri().query().map(str::to_owned);

    if let Ok(upgrade) = websocket {
        let url = upstream_url(server.url.as_str(), &rest, query.as_deref(), true);
        return upgrade.on_upgrade(move |client| relay_websocket(client, url));
    }

    let mount = mount(&state.paths.ui_route(), server_id);
    let url = upstream_url(server.url.as_str(), &rest, query.as_deref(), false);
    let (parts, body) = request.into_parts();
    let entry = is_web_entry(&parts.method, &rest);
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if is_dropped_request_header(name) || (entry && is_entry_negotiation_header(name)) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    let method = if parts.method == Method::CONNECT {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    } else {
        parts.method
    };
    let upstream = state
        .streaming_client
        .request(method, &url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    let response = match upstream.send().await {
        Ok(response) => response,
        Err(error) => {
            warn!(server_id = %server_id, %error, "Pass-through request failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let rewrite_page = entry
        && response.status() == StatusCode::OK
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"));
    let mut builder = Response::builder().status(response.status());
    let out = builder.headers_mut().expect("fresh builder");
    for (name, value) in response.headers() {
        if is_dropped_response_header(name) {
            continue;
        }
        if name == header::LOCATION {
            if let Ok(location) = value.to_str() {
                if let Ok(rewritten) = HeaderValue::from_str(&rewrite_location(
                    location,
                    &mount,
                    server.url.as_str(),
                )) {
                    out.append(header::LOCATION, rewritten);
                    continue;
                }
            }
        }
        out.append(name.clone(), value.clone());
    }
    if rewrite_page {
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                warn!(server_id = %server_id, %error, "Could not read the backend's start page");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };
        let html = String::from_utf8_lossy(&body);
        let page = inject_script(&html, &panel_script(&server, &mount, &identity))
            .unwrap_or_else(|| {
                warn!(server_id = %server_id, "The backend's start page has no <head>: serving it unchanged");
                html.into_owned()
            });
        // The page now depends on who is asking: it must not be cached or revalidated.
        for name in [
            header::CONTENT_LENGTH,
            header::CONTENT_ENCODING,
            header::ETAG,
            header::LAST_MODIFIED,
            header::CACHE_CONTROL,
        ] {
            out.remove(name);
        }
        out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return builder
            .body(Body::from(page))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn to_upstream(message: ClientMessage) -> UpstreamMessage {
    match message {
        ClientMessage::Text(text) => UpstreamMessage::Text(text.as_str().into()),
        ClientMessage::Binary(bytes) => UpstreamMessage::Binary(bytes.to_vec().into()),
        ClientMessage::Ping(bytes) => UpstreamMessage::Ping(bytes.to_vec().into()),
        ClientMessage::Pong(bytes) => UpstreamMessage::Pong(bytes.to_vec().into()),
        ClientMessage::Close(frame) => UpstreamMessage::Close(frame.map(|frame| {
            UpstreamCloseFrame {
                code: CloseCode::from(frame.code),
                reason: frame.reason.as_str().into(),
            }
        })),
    }
}

fn to_client(message: UpstreamMessage) -> Option<ClientMessage> {
    Some(match message {
        UpstreamMessage::Text(text) => ClientMessage::Text(text.as_str().into()),
        UpstreamMessage::Binary(bytes) => ClientMessage::Binary(bytes.to_vec().into()),
        UpstreamMessage::Ping(bytes) => ClientMessage::Ping(bytes.to_vec().into()),
        UpstreamMessage::Pong(bytes) => ClientMessage::Pong(bytes.to_vec().into()),
        UpstreamMessage::Close(frame) => ClientMessage::Close(frame.map(|frame| CloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().into(),
        })),
        UpstreamMessage::Frame(_) => return None,
    })
}

/// Copy messages both ways until either side closes.
async fn relay_websocket(client: WebSocket, url: String) {
    let (upstream, _) = match tokio_tungstenite::connect_async(url.as_str()).await {
        Ok(connection) => connection,
        Err(error) => {
            warn!(%error, "Pass-through WebSocket could not reach the backend");
            return;
        }
    };
    let (mut to_backend, mut from_backend) = upstream.split();
    let (mut to_client_sink, mut from_client) = client.split();

    let up = async {
        while let Some(Ok(message)) = from_client.next().await {
            let closing = matches!(message, ClientMessage::Close(_));
            if to_backend.send(to_upstream(message)).await.is_err() || closing {
                break;
            }
        }
    };
    let down = async {
        while let Some(Ok(message)) = from_backend.next().await {
            let closing = matches!(message, UpstreamMessage::Close(_));
            if let Some(message) = to_client(message) {
                if to_client_sink.send(message).await.is_err() {
                    break;
                }
            }
            if closing {
                break;
            }
        }
    };
    tokio::select! {
        () = up => {},
        () = down => {},
    }
    debug!("Pass-through WebSocket closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_is_pulled_back_into_the_mount() {
        let mount = mount("ui", ServerId::new(3));
        assert_eq!(mount, "/ui/backend/3");
        let base = "http://192.0.2.10:8096";
        assert_eq!(
            rewrite_location("/web/index.html", &mount, base),
            "/ui/backend/3/web/index.html"
        );
        assert_eq!(
            rewrite_location("http://192.0.2.10:8096/web/", &mount, base),
            "/ui/backend/3/web/"
        );
        assert_eq!(
            rewrite_location("http://192.0.2.10:8096", &mount, base),
            "/ui/backend/3"
        );
        // Not the backend and not a local path: left alone.
        assert_eq!(
            rewrite_location("https://example.com/x", &mount, base),
            "https://example.com/x"
        );
        assert_eq!(rewrite_location("//evil.example/x", &mount, base), "//evil.example/x");
        // A host that merely starts like the backend's is not the backend.
        assert_eq!(
            rewrite_location("http://192.0.2.10:80961/x", &mount, base),
            "http://192.0.2.10:80961/x"
        );
    }

    #[test]
    fn upstream_urls_keep_path_and_query_and_switch_scheme_for_websockets() {
        assert_eq!(
            upstream_url("http://nas:8096/", "web/index.html", Some("a=1&b=2"), false),
            "http://nas:8096/web/index.html?a=1&b=2"
        );
        assert_eq!(
            upstream_url("https://nas", "socket", Some("api_key=x"), true),
            "wss://nas/socket?api_key=x"
        );
        assert_eq!(upstream_url("http://nas:8096", "x", None, true), "ws://nas:8096/x");
        assert_eq!(upstream_url("http://nas:8096", "x", Some(""), false), "http://nas:8096/x");
    }

    #[test]
    fn dot_segments_are_refused_even_when_encoded() {
        assert!(has_dot_segments("../etc"));
        assert!(has_dot_segments("a/../b"));
        assert!(has_dot_segments("a/%2e%2e/b"));
        assert!(has_dot_segments("a\\..\\b"));
        assert!(!has_dot_segments("web/index.html"));
        assert!(!has_dot_segments("a/..b/c"));
    }

    #[test]
    fn cookies_never_cross_the_proxy() {
        assert!(is_dropped_request_header(&header::COOKIE));
        assert!(is_dropped_request_header(&header::HOST));
        assert!(!is_dropped_request_header(&header::AUTHORIZATION));
        assert!(!is_dropped_request_header(&HeaderName::from_static("x-emby-authorization")));
        assert!(is_dropped_response_header(&header::SET_COOKIE));
        assert!(!is_dropped_response_header(&header::CONTENT_TYPE));
    }
}

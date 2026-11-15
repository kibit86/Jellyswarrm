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
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
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
    upstream::headers::{prepare_request_headers, remove_hop_by_hop_headers},
};

/// What the browser sent, minus what must not travel to the backend: the shared
/// transport rules (hop-by-hop fields, `Host`, `Accept-Encoding`: the HTTP client
/// asks for, and decodes, its own), plus the proxy's session cookie, framing the
/// client sets itself, the WebSocket handshake fields (the relay makes its own),
/// and, for the start page, whatever would make the backend answer with something
/// other than a plain, complete page.
fn backend_request_headers(received: &HeaderMap, is_start_page: bool) -> HeaderMap {
    let mut headers = received.clone();
    prepare_request_headers(&mut headers);
    for name in [
        "cookie",
        "content-length",
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-extensions",
        "sec-websocket-accept",
    ] {
        headers.remove(name);
    }
    if is_start_page {
        for name in ["if-none-match", "if-modified-since", "range", "if-range"] {
            headers.remove(name);
        }
    }
    headers
}

/// What the backend answered, minus what must not reach the browser: hop-by-hop
/// fields, and the backend's cookies (they would collide with the proxy's).
fn browser_response_headers(received: &HeaderMap) -> HeaderMap {
    let mut headers = received.clone();
    remove_hop_by_hop_headers(&mut headers);
    headers.remove(header::SET_COOKIE);
    headers.remove("set-cookie2");
    headers
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
fn panel_script(server: &Server, mount: &str, entry: &Entry) -> String {
    // No session to hand over without a mapped account: the page asks, or the wizard creates one.
    let seed = match entry {
        Entry::Account(identity) => serde_json::json!({
            "Id": identity.backend_server_id,
            "Name": server.name,
            "LastConnectionMode": 2,
            "UserId": identity.user_id,
            "AccessToken": identity.token,
        }),
        Entry::Manual => serde_json::Value::Null,
    };
    PANEL_SCRIPT
        .replace(
            "%PREFIX%",
            &json_for_script(&format!("jsw-backend-{}:", server.id)),
        )
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

/// How the user is let into a server.
enum Entry {
    /// Signed in with the account they mapped there, which administers it.
    Account(std::sync::Arc<PanelIdentity>),
    /// The proxy administrator opening a server they have not connected an account to
    /// (a brand-new one needs its first-run wizard; any other, its own login page).
    /// Nothing is signed in for them: the server's page asks, and the server decides
    /// what the account they type may do.
    Manual,
}

/// Who may open `server_id`, and how:
/// - a user with a mapped account: only if that account administers the server, signed
///   in automatically;
/// - the proxy administrator, on a server with no mapped account yet: anyway, signing in
///   on the server's own page (this is how a new server gets its first account);
/// - everyone else: no.
async fn authorize(
    state: &ManagementState,
    user: &crate::ui::auth::UiUser,
    server_id: ServerId,
) -> Result<(Server, Entry), StatusCode> {
    let server = state
        .server_storage
        .get_server_by_id(server_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    match state.panel_identities.get(state, &user.id, &server).await {
        Ok(identity) if identity.is_administrator => Ok((server, Entry::Account(identity))),
        Ok(_) => Err(StatusCode::FORBIDDEN),
        Err(crate::ui::backend_identity::IdentityError::NotConnected)
            if user.role == crate::ui::auth::UserRole::Admin =>
        {
            Ok((server, Entry::Manual))
        }
        Err(error) => Err(StatusCode::from(error)),
    }
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
    let (server, entry) = match authorize(&state, &user, server_id).await {
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
    let is_start_page = is_web_entry(&parts.method, &rest);
    let completes_setup = parts.method == Method::POST
        && rest
            .trim_matches('/')
            .eq_ignore_ascii_case("Startup/Complete");
    let headers = backend_request_headers(&parts.headers, is_start_page);
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

    if completes_setup && response.status().is_success() {
        state.panel_identities.forget_setup(&server).await;
    }
    let rewrite_page = is_start_page
        && response.status() == StatusCode::OK
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"));
    let mut builder = Response::builder().status(response.status());
    let out = builder.headers_mut().expect("fresh builder");
    for (name, value) in &browser_response_headers(response.headers()) {
        if name == header::LOCATION {
            if let Ok(location) = value.to_str() {
                if let Ok(rewritten) =
                    HeaderValue::from_str(&rewrite_location(location, &mount, server.url.as_str()))
                {
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
        let page = inject_script(&html, &panel_script(&server, &mount, &entry))
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
        ClientMessage::Close(frame) => {
            UpstreamMessage::Close(frame.map(|frame| UpstreamCloseFrame {
                code: CloseCode::from(frame.code),
                reason: frame.reason.as_str().into(),
            }))
        }
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
    use axum::http::HeaderName;

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
        assert_eq!(
            rewrite_location("//evil.example/x", &mount, base),
            "//evil.example/x"
        );
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
        assert_eq!(
            upstream_url("http://nas:8096", "x", None, true),
            "ws://nas:8096/x"
        );
        assert_eq!(
            upstream_url("http://nas:8096", "x", Some(""), false),
            "http://nas:8096/x"
        );
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
    fn only_the_web_clients_start_page_is_rewritten() {
        let get = Method::GET;
        for rest in ["web", "web/", "web/index.html"] {
            assert!(is_web_entry(&get, rest), "{rest}");
        }
        for rest in [
            "web/main.bundle.js",
            "web/index.html.map",
            "System/Info",
            "",
            "web/sub/index.html",
        ] {
            assert!(!is_web_entry(&get, rest), "{rest}");
        }
        assert!(!is_web_entry(&Method::POST, "web/"));
        // The start page is fetched whole and unconditionally; other requests keep their conditions.
        let mut browser = HeaderMap::new();
        for (name, value) in [
            ("if-none-match", "\"x\""),
            ("range", "bytes=0-9"),
            ("authorization", "t"),
        ] {
            browser.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let page = backend_request_headers(&browser, true);
        assert!(page.get("if-none-match").is_none() && page.get("range").is_none());
        assert!(page.get("authorization").is_some());
        let asset = backend_request_headers(&browser, false);
        assert!(asset.get("if-none-match").is_some() && asset.get("range").is_some());
    }

    #[test]
    fn the_script_goes_first_in_head_whatever_its_case_or_attributes() {
        for html in [
            "<html><head><title>x</title></head></html>",
            "<HTML><HEAD lang=\"en\"><title>x</title></HEAD></HTML>",
        ] {
            let page = inject_script(html, "RUN()").expect("has a <head>");
            let head_end = page.find('>').unwrap();
            assert!(page.contains("<script>RUN()</script>"), "{page}");
            assert!(
                page.find("<script>").unwrap() > page.to_ascii_lowercase().find("<head").unwrap()
            );
            assert!(
                page.find("<script>").unwrap() < page.find("<title>").unwrap(),
                "before everything else"
            );
            assert!(page.len() > html.len() && head_end > 0);
        }
        assert!(inject_script("<html><body>no head</body></html>", "RUN()").is_none());
    }

    #[test]
    fn inline_json_cannot_close_the_script_element() {
        let text = json_for_script(&"</script><script>alert(1)</script>");
        assert!(!text.contains('<'), "{text}");
        let value: String = serde_json::from_str(&text).unwrap();
        assert_eq!(value, "</script><script>alert(1)</script>");
    }

    #[test]
    fn cookies_never_cross_the_proxy() {
        let mut browser = HeaderMap::new();
        for (name, value) in [
            ("cookie", "proxy-session=secret"),
            ("host", "proxy.lan"),
            ("accept-encoding", "gzip, br, zstd"),
            ("connection", "keep-alive, x-nominated"),
            ("x-nominated", "1"),
            ("authorization", "MediaBrowser Token=\"t\""),
            ("x-emby-authorization", "MediaBrowser Token=\"t\""),
            ("sec-websocket-key", "k"),
        ] {
            browser.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let to_backend = backend_request_headers(&browser, false);
        for gone in [
            "cookie",
            "host",
            "accept-encoding",
            "connection",
            "x-nominated",
            "sec-websocket-key",
        ] {
            assert!(
                to_backend.get(gone).is_none(),
                "{gone} must not reach the backend"
            );
        }
        assert!(to_backend.get("authorization").is_some());
        assert!(to_backend.get("x-emby-authorization").is_some());

        let mut backend = HeaderMap::new();
        for (name, value) in [
            ("set-cookie", "a=1"),
            ("set-cookie2", "b=2"),
            ("content-type", "text/html"),
            ("transfer-encoding", "chunked"),
        ] {
            backend.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let to_browser = browser_response_headers(&backend);
        assert!(to_browser.get("set-cookie").is_none() && to_browser.get("set-cookie2").is_none());
        assert!(to_browser.get("transfer-encoding").is_none());
        assert_eq!(to_browser["content-type"], "text/html");
    }
}

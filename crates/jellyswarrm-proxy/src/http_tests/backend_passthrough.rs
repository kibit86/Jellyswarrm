//! `/<ui>/backend/{server_id}/...`: one backend served unchanged, only to users
//! who are administrators on that backend.
use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use wiremock::{
    matchers::{body_string, header_exists, method, path},
    Mock, ResponseTemplate,
};

fn plain_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Sign in to the management UI; returns the session cookie (`name=value`).
async fn ui_login(f: &Fixture, username: &str, password: &str) -> String {
    let response = plain_client()
        .post(format!("{}/ui/login", f.url))
        .form(&[("username", username), ("password", password)])
        .send()
        .await
        .unwrap();
    assert!(response.status().is_redirection(), "{}", response.status());
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("; ")
}

async fn server_id(f: &Fixture, index: usize) -> i64 {
    f.state
        .server_storage
        .list_servers()
        .await
        .unwrap()
        .into_iter()
        .find(|server| server.url.as_str().starts_with(&f.upstreams[index].uri()))
        .unwrap()
        .id
        .as_i64()
}

fn backend(f: &Fixture, id: i64, rest: &str) -> String {
    format!("{}/ui/backend/{id}/{rest}", f.url)
}

#[tokio::test]
async fn signed_out_visitors_never_reach_a_backend() {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    let id = server_id(&f, 1).await;
    let response = plain_client()
        .get(backend(&f, id, "web/"))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_redirection() || response.status() == StatusCode::UNAUTHORIZED,
        "{}",
        response.status()
    );
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn only_administrators_of_that_backend_get_in() {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    f.healthy().await;
    f.administrator_on(0, false).await;
    f.administrator_on(1, true).await;
    for index in 0..2 {
        Mock::given(method("GET"))
            .and(path("/web/index.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("backend {index}")))
            .mount(&f.upstreams[index])
            .await;
    }
    let cookie = ui_login(&f, "alice", "password").await;
    let (not_admin, admin) = (server_id(&f, 0).await, server_id(&f, 1).await);

    let refused = plain_client()
        .get(backend(&f, not_admin, "web/index.html"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(f.requests_without_admin_lookup(0).await.is_empty());

    let allowed = plain_client()
        .get(backend(&f, admin, "web/index.html"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(allowed.text().await.unwrap(), "backend 1");

    let unknown = plain_client()
        .get(backend(&f, 9999, "web/index.html"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // Someone with no linked account on any backend is refused too.
    let other_cookie = ui_login(&f, "bob", "password").await;
    let stranger = plain_client()
        .get(backend(&f, admin, "web/index.html"))
        .header("cookie", &other_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(stranger.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn requests_and_responses_pass_unchanged_except_cookies_and_redirects() {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    f.healthy().await;
    f.administrator_on(1, true).await;
    let backend_mock = &f.upstreams[1];
    Mock::given(method("POST"))
        .and(path("/Items/some-item"))
        .and(body_string(r#"{"Name":"New"}"#))
        .and(header_exists("authorization"))
        .respond_with(
            ResponseTemplate::new(204)
                .insert_header("set-cookie", "backend=1; Path=/")
                .insert_header("x-backend", "yes"),
        )
        .expect(1)
        .mount(backend_mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/web/"))
        .mount(backend_mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/System/Info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Id": "real-server-id"})))
        .mount(backend_mock)
        .await;

    let cookie = ui_login(&f, "alice", "password").await;
    let id = server_id(&f, 1).await;

    let response = plain_client()
        .post(backend(&f, id, "Items/some-item"))
        .header("cookie", &cookie)
        .header("authorization", r#"MediaBrowser Token="backend-session""#)
        .header("content-type", "application/json")
        .body(r#"{"Name":"New"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["x-backend"], "yes");
    assert!(
        response.headers().get("set-cookie").is_none(),
        "the backend's cookies must not reach the browser"
    );
    let seen = f.requests_without_admin_lookup(1).await;
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].headers.get("cookie").is_none(),
        "the proxy's session cookie must not reach the backend"
    );
    assert_eq!(
        seen[0].headers.get("authorization").unwrap().to_str().unwrap(),
        r#"MediaBrowser Token="backend-session""#
    );

    // Redirects stay inside the mount.
    let redirect = plain_client()
        .get(backend(&f, id, "old"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), StatusCode::FOUND);
    assert_eq!(
        redirect.headers()["location"],
        format!("/proxy/ui/backend/{id}/web/")
    );

    // The bare address is a shortcut to the backend's web client.
    let bare = plain_client()
        .get(backend(&f, id, ""))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        bare.headers()["location"],
        format!("/proxy/ui/backend/{id}/web/")
    );

    // Not translated: the backend's own identity, not the proxy's.
    let info: Value = plain_client()
        .get(backend(&f, id, "System/Info"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["Id"], "real-server-id");
}

#[tokio::test]
async fn websocket_is_relayed_both_ways() {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;

    // A backend that only speaks WebSocket (echo, with a greeting).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.send(Message::text("hello from backend")).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if message.is_text() {
                socket.send(message).await.unwrap();
            } else if message.is_close() {
                break;
            }
        }
    });

    let server = {
        let id = f
            .state
            .server_storage
            .add_server(
                "WebSocket backend",
                &format!("http://{backend_addr}"),
                100,
                MediaStreamingMode::Proxy,
            )
            .await
            .unwrap();
        f.state
            .server_storage
            .get_server_by_id(id)
            .await
            .unwrap()
            .unwrap()
    };
    f.state
        .user_authorization
        .add_server_mapping(&f.user.id, &server, "alice", &"password".into(), None)
        .await
        .unwrap();
    let authorization = Authorization {
        client: "Test".into(),
        device: "Device ws".into(),
        device_id: "ws".into(),
        version: "1".into(),
        token: None,
    };
    f.state
        .user_authorization
        .store_authorization_session(
            &f.user.id,
            &server,
            &authorization,
            "ws-backend-token".into(),
            "ws-backend-user".into(),
            None,
        )
        .await
        .unwrap();
    // The backend is not an HTTP server here, so state that the user is an
    // administrator on it the way a backend login would.
    f.state
        .services
        .admin_rights
        .record("ws-backend-token", true)
        .await;

    let cookie = ui_login(&f, "alice", "password").await;
    let url = format!(
        "ws://{}/ui/backend/{}/socket?api_key=x",
        f.url.trim_start_matches("http://"),
        server.id
    );
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert("cookie", cookie.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();

    let greeting = socket.next().await.unwrap().unwrap();
    assert_eq!(greeting.into_text().unwrap().as_str(), "hello from backend");
    socket.send(Message::text("ping from client")).await.unwrap();
    let echoed = socket.next().await.unwrap().unwrap();
    assert_eq!(echoed.into_text().unwrap().as_str(), "ping from client");
}

#[tokio::test]
async fn websocket_without_a_session_is_refused() {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    let id = server_id(&f, 1).await;
    let url = format!(
        "ws://{}/ui/backend/{id}/socket",
        f.url.trim_start_matches("http://")
    );
    assert!(tokio_tungstenite::connect_async(url).await.is_err());
}

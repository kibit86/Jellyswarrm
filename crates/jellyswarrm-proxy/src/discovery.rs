//! LAN auto-discovery, the way a Jellyfin server does it.
//!
//! Jellyfin apps find servers by broadcasting the text `who is JellyfinServer?`
//! to UDP port 7359; every server answers the sender with a small JSON
//! description. This module answers for the proxy so apps list it like any
//! Jellyfin server.
//!
//! Only one server per host may own the port, and that is deliberate: when
//! another server (for example a Jellyfin on the same machine) already holds
//! it, the proxy logs the problem once and keeps running without discovery,
//! trying again later in case the port is freed. The setting `autodiscovery`
//! turns the proxy's answers on or off without a restart.
use std::{net::SocketAddr, time::Duration};

use serde::Serialize;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

use crate::config::SettingsView;

/// The port Jellyfin clients broadcast to.
pub(crate) const DISCOVERY_PORT: u16 = 7359;
/// How often the configuration is re-read, and a failed bind retried.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const REQUEST_MARKER: &str = "who is jellyfinserver?";
/// Datagrams are tiny; never read more than this.
const MAX_DATAGRAM: usize = 512;

/// What a Jellyfin server answers (same field names as Jellyfin's
/// `ServerDiscoveryInfo`).
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
struct DiscoveryInfo<'a> {
    address: String,
    id: &'a str,
    name: &'a str,
    endpoint_address: Option<String>,
}

/// Jellyfin matches the marker anywhere in the datagram, ignoring case.
pub(crate) fn is_discovery_request(datagram: &[u8]) -> bool {
    String::from_utf8_lossy(datagram)
        .to_ascii_lowercase()
        .contains(REQUEST_MARKER)
}

/// The JSON answer for the current settings. The address must be a full URL:
/// clients connect to exactly what is announced.
pub(crate) fn discovery_response(public_address: &str, server_id: &str, server_name: &str) -> Vec<u8> {
    let address = if public_address.contains("://") {
        public_address.to_owned()
    } else {
        format!("http://{public_address}")
    };
    serde_json::to_vec(&DiscoveryInfo {
        address,
        id: server_id,
        name: server_name,
        endpoint_address: None,
    })
    .expect("discovery info serializes")
}

async fn bind(port: u16) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    socket.set_broadcast(true)?;
    Ok(socket)
}

/// Answer requests on `socket` for at most `window`, then return so the caller
/// can re-read the configuration. `Err` means the socket is unusable.
async fn serve_for(
    socket: &UdpSocket,
    settings: &SettingsView,
    window: Duration,
) -> std::io::Result<()> {
    let serve = async {
        let mut buffer = [0u8; MAX_DATAGRAM];
        loop {
            let (length, peer) = socket.recv_from(&mut buffer).await?;
            if !is_discovery_request(&buffer[..length]) {
                continue;
            }
            // Read per request so renames and address changes apply at once.
            let current = settings.read().await;
            let reply =
                discovery_response(&current.public_address, &current.server_id, &current.server_name);
            drop(current);
            if let Err(error) = socket.send_to(&reply, peer).await {
                warn!(%peer, %error, "Could not answer discovery request");
            } else {
                debug!(%peer, "Answered discovery request");
            }
        }
    };
    match tokio::time::timeout(window, serve).await {
        Ok(result) => result,
        Err(_elapsed) => Ok(()),
    }
}

/// Own the discovery socket according to the `autodiscovery` setting, forever.
/// Never panics and never stops the application: a busy port or a socket error
/// only disables discovery until the next attempt.
pub(crate) async fn supervise(settings: SettingsView, port: u16, poll: Duration) {
    let mut socket: Option<UdpSocket> = None;
    // Log a failure once, not on every retry.
    let mut failure_reported = false;
    loop {
        let enabled = settings.read().await.autodiscovery;
        match (enabled, socket.is_some()) {
            (false, true) => {
                info!("Network discovery disabled; releasing UDP port {port}");
                socket = None;
                failure_reported = false;
            }
            (true, false) => match bind(port).await {
                Ok(bound) => {
                    info!("Answering network discovery on UDP port {port}");
                    socket = Some(bound);
                    failure_reported = false;
                }
                Err(error) => {
                    if !failure_reported {
                        error!(
                            %error,
                            "Network discovery is off: could not listen on UDP port {port} \
                             (another server may own it; disable its discovery or set \
                             autodiscovery = false here). The proxy keeps running and will retry."
                        );
                        failure_reported = true;
                    }
                }
            },
            (false, false) => failure_reported = false,
            (true, true) => {}
        }
        match &socket {
            Some(bound) => {
                if let Err(error) = serve_for(bound, &settings, poll).await {
                    error!(%error, "Network discovery socket failed; will reopen it");
                    socket = None;
                    failure_reported = false;
                }
            }
            None => tokio::time::sleep(poll).await,
        }
    }
}

/// Production entry point.
pub(crate) async fn run(settings: SettingsView) {
    supervise(settings, DISCOVERY_PORT, POLL_INTERVAL).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn settings(autodiscovery: bool) -> (Arc<RwLock<AppConfig>>, SettingsView) {
        let config = Arc::new(RwLock::new(AppConfig {
            server_id: "abc123".into(),
            server_name: "Proxy".into(),
            public_address: "http://192.168.1.1:8097".into(),
            autodiscovery,
            ..AppConfig::default()
        }));
        let view = SettingsView::new(config.clone());
        (config, view)
    }

    async fn free_udp_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Ask on loopback and wait briefly for an answer.
    async fn ask(port: u16, text: &str) -> Option<String> {
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(text.as_bytes(), ("127.0.0.1", port)).await.unwrap();
        let mut buffer = [0u8; 1024];
        match tokio::time::timeout(Duration::from_millis(600), client.recv_from(&mut buffer)).await
        {
            Ok(Ok((length, _))) => Some(String::from_utf8_lossy(&buffer[..length]).into_owned()),
            _ => None,
        }
    }

    #[test]
    fn recognises_the_jellyfin_request_in_any_case_and_inside_other_bytes() {
        assert!(is_discovery_request(b"who is JellyfinServer?"));
        assert!(is_discovery_request(b"WHO IS JELLYFINSERVER?"));
        assert!(is_discovery_request(b"\x00\x01 who is JellyfinServer? trailing"));
        assert!(!is_discovery_request(b"who is EmbyServer?"));
        assert!(!is_discovery_request(b""));
        assert!(!is_discovery_request(&[0xff, 0xfe, 0xfd]));
    }

    #[test]
    fn answer_has_jellyfins_field_names_and_a_full_url() {
        let reply = discovery_response("http://192.168.1.1:8097", "abc123", "Proxy");
        let value: serde_json::Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(value["Address"], "http://192.168.1.1:8097");
        assert_eq!(value["Id"], "abc123");
        assert_eq!(value["Name"], "Proxy");
        assert!(value["EndpointAddress"].is_null());

        let bare = discovery_response("nas.lan:8097", "abc123", "Proxy");
        let value: serde_json::Value = serde_json::from_slice(&bare).unwrap();
        assert_eq!(value["Address"], "http://nas.lan:8097");
    }

    #[tokio::test]
    async fn answers_a_real_request_and_ignores_other_traffic() {
        let (_config, view) = settings(true);
        let port = free_udp_port().await;
        let task = tokio::spawn(supervise(view, port, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let reply = ask(port, "who is JellyfinServer?").await.expect("an answer");
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["Id"], "abc123");
        assert_eq!(value["Address"], "http://192.168.1.1:8097");
        assert!(ask(port, "hello").await.is_none());
        task.abort();
    }

    #[tokio::test]
    async fn can_be_switched_off_and_on_without_a_restart() {
        let (config, view) = settings(true);
        let port = free_udp_port().await;
        let task = tokio::spawn(supervise(view, port, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(ask(port, "who is JellyfinServer?").await.is_some());

        config.write().await.autodiscovery = false;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            ask(port, "who is JellyfinServer?").await.is_none(),
            "disabled: no answer"
        );
        // The port is really released, so another server could take it.
        drop(UdpSocket::bind(("0.0.0.0", port)).await.expect("port released"));

        config.write().await.autodiscovery = true;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(ask(port, "who is JellyfinServer?").await.is_some());
        task.abort();
    }

    #[tokio::test]
    async fn a_name_change_is_announced_without_a_restart() {
        let (config, view) = settings(true);
        let port = free_udp_port().await;
        let task = tokio::spawn(supervise(view, port, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(200)).await;

        config.write().await.server_name = "Renamed".into();
        let reply = ask(port, "who is JellyfinServer?").await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).unwrap()["Name"],
            "Renamed"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_busy_port_is_survived_and_taken_over_once_free() {
        let (_config, view) = settings(true);
        // Another "server" owns the port.
        let squatter = UdpSocket::bind(("0.0.0.0", 0)).await.unwrap();
        let port = squatter.local_addr().unwrap().port();

        let task = tokio::spawn(supervise(view, port, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!task.is_finished(), "a busy port must not stop the task");

        // Nobody answers while the squatter holds the port and ignores requests.
        assert!(ask(port, "who is JellyfinServer?").await.is_none());

        // When the port is freed, discovery starts by itself.
        drop(squatter);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(ask(port, "who is JellyfinServer?").await.is_some());
        task.abort();
    }

    #[tokio::test]
    async fn disabled_from_the_start_never_opens_the_port() {
        let (_config, view) = settings(false);
        let port = free_udp_port().await;
        let task = tokio::spawn(supervise(view, port, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(UdpSocket::bind(("0.0.0.0", port)).await.expect("port untouched"));
        task.abort();
    }
}

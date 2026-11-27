//! One access-log line per request, under the `tablica::access` target.
//!
//! Each line names the device, the client address, the outcome and the device's
//! telemetry headers, and, for the setup and display calls, the image URL handed
//! to the device: a device that talks to the API but cannot fetch that URL shows
//! up as a display line with no image request after it.
//!
//! What a handler decided travels back in an [`Access`] response extension, so
//! the line never second-guesses it: the device it served, or why it refused.
//! A refused request logs one WARN here and nothing in the handler.
//!
//! Never logged: the `Access-Token` header, the query string (a misconfigured
//! client may put a key there), or an `ID` header that is not a MAC.

use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tracing::{debug, info, warn};

use super::{AppState, authenticate, header_str};

/// What a handler tells the access log about the request it answered.
#[derive(Clone, Default)]
pub(super) struct Access {
    /// The configured device the handler resolved and served.
    pub device: Option<String>,
    /// Why the handler refused the request.
    pub reason: Option<String>,
    /// The image URL handed to the device.
    pub image_url: Option<String>,
}

impl Access {
    pub fn served(device: &str, image_url: Option<String>) -> Self {
        Self {
            device: Some(device.to_owned()),
            reason: None,
            image_url,
        }
    }

    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::default()
        }
    }
}

/// `mac` in canonical `AA:BB:CC:DD:EE:FF` form when it is six hex octets,
/// optionally separated by `:` or `-`; anything else is `None`.
pub(super) fn valid_mac(mac: &str) -> Option<String> {
    let hex: String = mac.chars().filter(|c| !matches!(c, ':' | '-')).collect();
    let separators = mac.len() - hex.len();
    if hex.len() != 12
        || !hex.chars().all(|c| c.is_ascii_hexdigit())
        || !matches!(separators, 0 | 5)
    {
        return None;
    }
    let upper = hex.to_ascii_uppercase();
    let octets: Vec<&str> = (0..6).map(|i| &upper[i * 2..i * 2 + 2]).collect();
    Some(octets.join(":"))
}

/// The TCP peer, and the client it stands for. Only a loopback peer (the DSM
/// reverse proxy; the port is bound to loopback) is trusted to name the client,
/// through the rightmost `X-Forwarded-For` hop: the proxy appends the address it
/// saw, and anything to the left of it came from the caller.
fn addresses(req: &Request<Body>) -> (Option<IpAddr>, Option<IpAddr>) {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let forwarded = || {
        header_str(req.headers(), "X-Forwarded-For")?
            .rsplit(',')
            .next()?
            .trim()
            .parse::<IpAddr>()
            .ok()
    };
    let client = match peer {
        Some(p) if p.is_loopback() => forwarded().or(peer),
        _ => peer,
    };
    (peer, client)
}

pub(super) async fn layer(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let headers = req.headers().clone();
    let (peer, client) = addresses(&req);

    let response = next.run(req).await;

    let h = |name| header_str(&headers, name);
    let mac = h("ID").and_then(valid_mac);
    let note = response.extensions().get::<Access>();
    // A handler's word wins; routes without one (images) fall back to the token.
    let device = match note {
        Some(n) => n.device.as_deref(),
        None => authenticate(&state.cfg, &headers).map(|d| d.name.as_str()),
    };
    let reason = note.and_then(|n| n.reason.as_deref());
    let image_url = note.and_then(|n| n.image_url.as_deref());
    let status = response.status();
    let bytes = response.body().size_hint().exact();
    let elapsed_ms = started.elapsed().as_millis() as u64;

    macro_rules! access {
        ($level:ident) => {
            $level!(
                target: "tablica::access",
                client = client.map(tracing::field::display),
                peer = peer.map(tracing::field::display),
                device,
                mac,
                status = status.as_u16(),
                reason,
                elapsed_ms,
                bytes,
                image_url,
                rssi = h("RSSI"),
                battery_voltage = h("Battery-Voltage"),
                fw_version = h("FW-Version"),
                model = h("Model"),
                user_agent = h("User-Agent"),
                "{method} {path} {}",
                status.as_u16(),
            )
        };
    }
    if status.is_client_error() || status.is_server_error() {
        access!(warn);
    } else if path == "/healthz" {
        // Polled by health checks; not worth a line each time.
        access!(debug);
    } else {
        access!(info);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::valid_mac;

    #[test]
    fn valid_mac_accepts_six_octets_only() {
        for ok in ["aa:bb:cc:dd:ee:01", "AA-BB-CC-DD-EE-01", "aabbccddee01"] {
            assert_eq!(valid_mac(ok).as_deref(), Some("AA:BB:CC:DD:EE:01"), "{ok}");
        }
        for bad in [
            "",
            "secret-token-1234",
            "AA:BB:CC:DD:EE",
            "AA:BB:CC:DD:EE:01:02",
            "AA:BB:CC:DD:EE:0G",
            "AABB:CCDDEE01",
            "0123456789ab0123456789ab",
        ] {
            assert_eq!(valid_mac(bad), None, "{bad}");
        }
    }
}

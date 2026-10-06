//! Loopback-only guard (`TASKS.md` T5.1, issue #18 — "非 localhost 連線被
//! 拒"). Binding `TcpListener` to `127.0.0.1` already makes the daemon
//! unreachable from another host at the OS level; this middleware is
//! defense in depth against a future change accidentally widening the
//! bind address (e.g. `0.0.0.0` for convenience) — the loopback check is
//! then the only thing standing between the write gate and the network.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Whether `addr`'s IP is loopback — the entire IPv4 `127.0.0.0/8` block
/// (not just `127.0.0.1`) and IPv6 `::1`, matching `IpAddr::is_loopback`'s
/// own definition.
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Reject any request whose peer address isn't loopback with `403`, before
/// it reaches any route handler.
pub async fn loopback_only(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if !is_loopback(&addr) {
        return (
            StatusCode::FORBIDDEN,
            "serialwarden web GUI only accepts connections from localhost; use `ssh -L` for \
             remote access",
        )
            .into_response();
    }
    next.run(request).await
}

/// Whether a host name (no port, IPv6 still bracketed) names this machine's
/// loopback interface: `localhost`, any `127.0.0.0/8` literal, or `[::1]`.
fn is_loopback_host_name(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|ip| ip.is_loopback());
    }
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Strip an optional `:port` from a `Host`-style authority, leaving IPv6
/// brackets in place.
fn host_without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return match authority.find(']') {
            Some(end) => &authority[..=end],
            None => authority,
        };
    }
    authority.split(':').next().unwrap_or(authority)
}

/// `Host: localhost:5590`, `Host: 127.0.0.1:15590`, `Host: [::1]` — any
/// port, since an `ssh -L` tunnel's local port needn't match the daemon's.
pub fn is_loopback_host_header(value: &str) -> bool {
    is_loopback_host_name(host_without_port(value.trim()))
}

/// `Origin: http://127.0.0.1:5590` and friends. `null` (sandboxed frames,
/// `file://` pages) and every non-loopback origin are rejected.
pub fn is_loopback_origin(value: &str) -> bool {
    let value = value.trim();
    let rest = match value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    {
        Some(rest) => rest,
        None => return false,
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    is_loopback_host_name(host_without_port(authority))
}

/// Reject requests a browser makes on behalf of some *other* website.
///
/// [`loopback_only`] checks the TCP peer, which a browser on this machine
/// always passes — so on its own it lets any page the developer has open
/// reach the API through their browser. Three concrete attacks that left
/// open, all verified against a real daemon before this guard existed:
///
/// - **Cross-site request forgery.** `POST /api/approvals/:id/approve`
///   takes no body, so a page's `fetch(url, {method: "POST", mode:
///   "no-cors"})` is a CORS "simple request" with no preflight: it
///   approved a pending `danger:erase` agent write. The approval gate is
///   the one control between an agent and an irreversible write, so a
///   third-party page must never be able to click it.
/// - **DNS rebinding.** A hostile domain re-pointed at `127.0.0.1` makes
///   the browser treat the daemon as that domain's own origin, with full
///   read and write access. The `Host` header still carries the hostile
///   name, which is what this checks.
/// - **Cross-site WebSocket hijacking.** Browsers apply no CORS to
///   WebSockets, so any page could subscribe to `/api/stream` and read the
///   device log live. The handshake carries `Origin`, which is checked.
///
/// Requests with no `Origin` (the CLI, curl, a same-origin `GET`) and no
/// `Host` (HTTP/1.0, in-process tests) pass: neither is something a
/// browser lets another site produce.
pub async fn same_site_only(request: Request, next: Next) -> Response {
    let headers = request.headers();
    if let Some(host) = headers.get(axum::http::header::HOST) {
        let ok = host.to_str().is_ok_and(is_loopback_host_header);
        if !ok {
            return (
                StatusCode::FORBIDDEN,
                "serialwarden web GUI only answers requests addressed to localhost / 127.0.0.1",
            )
                .into_response();
        }
    }
    if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
        let ok = origin.to_str().is_ok_and(is_loopback_origin);
        if !ok {
            return (
                StatusCode::FORBIDDEN,
                "serialwarden web GUI rejects requests from other websites",
            )
                .into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_host_headers_are_accepted_on_any_port() {
        for h in [
            "127.0.0.1:5590",
            "127.0.0.1",
            "localhost:15590",
            "LOCALHOST",
            "[::1]:5590",
            "[::1]",
            "127.4.5.6:1",
        ] {
            assert!(is_loopback_host_header(h), "{h} should be accepted");
        }
    }

    #[test]
    fn foreign_host_headers_are_rejected() {
        for h in [
            "attacker.example:5590",
            "attacker.example",
            "localhost.attacker.example",
            "127.0.0.1.nip.io:5590",
            "192.168.1.10:5590",
            "[2001:db8::1]:5590",
            "",
        ] {
            assert!(!is_loopback_host_header(h), "{h} should be rejected");
        }
    }

    #[test]
    fn loopback_origins_are_accepted_and_others_rejected() {
        for o in [
            "http://127.0.0.1:5590",
            "http://localhost:5173",
            "http://[::1]:5590",
            "https://localhost",
        ] {
            assert!(is_loopback_origin(o), "{o} should be accepted");
        }
        for o in [
            "https://evil.example",
            "http://localhost.evil.example",
            "null",
            "file://",
            "chrome-extension://abc",
            "http://10.0.0.2:5590",
        ] {
            assert!(!is_loopback_origin(o), "{o} should be rejected");
        }
    }

    /// Through the real router: the exact cross-site request that approved
    /// a pending `danger:erase` write before this guard existed.
    #[tokio::test]
    async fn cross_site_requests_never_reach_a_handler() {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        use crate::protocol::backend::testing::TestBackend;
        use crate::protocol::backend::DeviceBackend;
        use crate::protocol::Shared;

        let tmp = tempfile::tempdir().expect("tempdir");
        let shared = Arc::new(Shared::new(
            Arc::new(TestBackend::new()) as Arc<dyn DeviceBackend>,
            "test",
            tmp.path(),
        ));
        let router = crate::web::router(shared);
        let loopback = || ConnectInfo("127.0.0.1:9999".parse::<SocketAddr>().unwrap());

        let foreign_origin = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/approvals/1/approve")
                    .header("host", "127.0.0.1:5590")
                    .header("origin", "https://evil.example")
                    .header("content-type", "text/plain")
                    .extension(loopback())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(foreign_origin.status(), StatusCode::FORBIDDEN);

        let rebound_host = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/devices")
                    .header("host", "attacker.example:5590")
                    .extension(loopback())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rebound_host.status(), StatusCode::FORBIDDEN);

        let same_site = router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/health")
                    .header("host", "localhost:15590")
                    .header("origin", "http://localhost:15590")
                    .extension(loopback())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(same_site.status(), StatusCode::OK);
    }

    #[test]
    fn ipv4_loopback_block_is_accepted_not_just_127_0_0_1() {
        assert!(is_loopback(&"127.0.0.1:1234".parse().unwrap()));
        assert!(is_loopback(&"127.5.6.7:1".parse().unwrap()));
        assert!(is_loopback(&"127.255.255.255:1".parse().unwrap()));
    }

    #[test]
    fn ipv6_loopback_is_accepted() {
        assert!(is_loopback(&"[::1]:1234".parse().unwrap()));
    }

    #[test]
    fn private_and_public_addresses_are_rejected() {
        assert!(!is_loopback(&"10.0.0.5:1234".parse().unwrap()));
        assert!(!is_loopback(&"192.168.1.1:1234".parse().unwrap()));
        assert!(!is_loopback(&"172.16.0.1:1234".parse().unwrap()));
        assert!(!is_loopback(&"8.8.8.8:1234".parse().unwrap()));
        assert!(!is_loopback(&"[2001:db8::1]:1234".parse().unwrap()));
    }

    /// End-to-end through the real router (not just the pure predicate
    /// above): a non-loopback `ConnectInfo` — the same extension type
    /// `axum::serve(...).into_make_service_with_connect_info::<SocketAddr>()`
    /// inserts from the real peer address — must never reach a route
    /// handler at all.
    #[tokio::test]
    async fn non_loopback_connect_info_is_rejected_before_any_route_handler() {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        use crate::protocol::backend::testing::TestBackend;
        use crate::protocol::backend::DeviceBackend;
        use crate::protocol::Shared;

        let tmp = tempfile::tempdir().expect("tempdir");
        let shared = Arc::new(Shared::new(
            Arc::new(TestBackend::new()) as Arc<dyn DeviceBackend>,
            "test",
            tmp.path(),
        ));
        let router = crate::web::router(shared);

        let rejected = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/health")
                    .extension(ConnectInfo(
                        "203.0.113.7:9999".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let allowed = router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/health")
                    .extension(ConnectInfo("127.0.0.1:9999".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }
}

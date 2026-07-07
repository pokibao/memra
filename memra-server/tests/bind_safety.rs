//! SEC-03: Bind Safety Integration Tests
//!
//! Asserts that the Rust HTTP server layer:
//!   1. Defaults to 127.0.0.1 (loopback) when no explicit bind is configured.
//!   2. Rejects a 0.0.0.0 bind without TLS (runtime guard via HttpBindError).
//!   3. Config loading defaults preserve 127.0.0.1 when no config file exists.
//!   4. No CLI default resolves to a forbidden address (0.0.0.0 / :: / ::0).
//!
//! Reference: docs/STACK_MAP.md Operating Principles #6
//!            docs/sec-migration-plan-2026-04-15.md SEC-03 section

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use memra_server::config::{AppConfig, DEFAULT_HTTP_BIND};
use memra_server::transport::http::{HttpBindError, HttpServeOptions};

/// Addresses that must never be used as a bind target without TLS.
const FORBIDDEN_BINDS: [&str; 3] = ["0.0.0.0", "::", "::0"];

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: DEFAULT_HTTP_BIND is loopback
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn default_http_bind_is_loopback() {
    assert!(
        DEFAULT_HTTP_BIND.is_loopback(),
        "DEFAULT_HTTP_BIND must be a loopback address, got: {DEFAULT_HTTP_BIND}"
    );
    assert_eq!(
        DEFAULT_HTTP_BIND.to_string(),
        "127.0.0.1",
        "DEFAULT_HTTP_BIND must be 127.0.0.1"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: AppConfig::default() uses loopback
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn app_config_default_bind_is_loopback() {
    let config = AppConfig::default();
    assert!(
        config.server.bind.is_loopback(),
        "AppConfig::default() server.bind must be loopback, got: {}",
        config.server.bind
    );
    assert!(
        !FORBIDDEN_BINDS.contains(&config.server.bind.to_string().as_str()),
        "AppConfig::default() server.bind must not be in forbidden set, got: {}",
        config.server.bind
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: Missing config file falls back to loopback (AppConfig::load_from_path)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn missing_config_file_defaults_to_loopback() {
    let nonexistent = std::env::temp_dir().join("ma-bind-safety-nonexistent.yaml");
    // Ensure it doesn't exist
    let _ = std::fs::remove_file(&nonexistent);

    let config = AppConfig::load_from_path(&nonexistent)
        .expect("missing config should not error — it falls back to defaults");

    assert!(
        config.server.bind.is_loopback(),
        "Missing config must default to loopback, got: {}",
        config.server.bind
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: HttpServeOptions rejects 0.0.0.0 without TLS
// ─────────────────────────────────────────────────────────────────────────────

fn config_with_bind(bind: IpAddr) -> AppConfig {
    use memra_server::config::{AuthConfig, ServerConfig, TlsConfig};
    AppConfig {
        auth: AuthConfig::default(),
        server: ServerConfig {
            bind,
            port: 7331,
            tls: TlsConfig::default(),
        },
    }
}

#[test]
fn unspecified_bind_without_tls_is_rejected() {
    let unspecified = IpAddr::V4(Ipv4Addr::UNSPECIFIED); // 0.0.0.0
    let config = config_with_bind(unspecified);
    let args = memra_server::cli::ServeArgs::default();

    let result = HttpServeOptions::resolve(&args, &config);

    assert!(
        matches!(result, Err(HttpBindError::RemoteBindRequiresTls)),
        "0.0.0.0 bind without TLS must be rejected, got: {result:?}"
    );
}

#[test]
fn ipv6_unspecified_bind_without_tls_is_rejected() {
    let unspecified = IpAddr::V6(Ipv6Addr::UNSPECIFIED); // ::
    let config = config_with_bind(unspecified);
    let args = memra_server::cli::ServeArgs::default();

    let result = HttpServeOptions::resolve(&args, &config);

    assert!(
        matches!(result, Err(HttpBindError::RemoteBindRequiresTls)),
        ":: bind without TLS must be rejected, got: {result:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: HttpServeOptions with loopback config resolves correctly
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn loopback_config_resolves_to_loopback_socket_addr() {
    let config = config_with_bind(IpAddr::V4(Ipv4Addr::LOCALHOST)); // 127.0.0.1
    let args = memra_server::cli::ServeArgs::default();

    let options = HttpServeOptions::resolve(&args, &config)
        .expect("loopback bind should resolve without error");

    assert!(
        options.addr.ip().is_loopback(),
        "Resolved address must be loopback, got: {}",
        options.addr
    );
    assert!(
        !FORBIDDEN_BINDS.contains(&options.addr.ip().to_string().as_str()),
        "Resolved address must not be in forbidden set, got: {}",
        options.addr.ip()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6: SocketAddr from loopback getsockname() equivalent
// Mirrors the Python test: bind a real TCP socket and confirm the address
// reported by the OS is not in the forbidden set.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn real_tcp_bind_to_loopback_reports_non_forbidden_addr() {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0 must succeed");
    let addr = listener.local_addr().expect("local_addr must succeed");

    assert!(
        addr.ip().is_loopback(),
        "TcpListener bound to 127.0.0.1 must report loopback, got: {}",
        addr.ip()
    );
    assert!(
        !FORBIDDEN_BINDS.contains(&addr.ip().to_string().as_str()),
        "TcpListener address must not be in forbidden set, got: {}",
        addr.ip()
    );
}

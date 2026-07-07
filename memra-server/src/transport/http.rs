use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::GlobalKeyExtractor;

use crate::audit::AuditLogger;
use crate::cli::ServeArgs;
use crate::config::AppConfig;
use crate::service::MemraService;
use crate::transport::auth::{AuthState, Authenticator, auth_middleware};
use crate::transport::idempotency::{IdempotencyLedger, IdempotencyState, idempotency_middleware};
use crate::transport::session::{SessionState, session_middleware};
use crate::transport::session_open::{SessionOpenState, session_open_handler};

pub const GLOBAL_HTTP_RATE_LIMIT_PER_MINUTE: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpServeOptions {
    pub addr: SocketAddr,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
}

impl HttpServeOptions {
    pub fn resolve(args: &ServeArgs, config: &AppConfig) -> Result<Self, HttpBindError> {
        let bind = args.bind_override().unwrap_or(config.server.bind);
        let port = args.port_override().unwrap_or(config.server.port);
        let tls_cert = args
            .tls_cert()
            .map(PathBuf::from)
            .or_else(|| config.server.tls.cert.clone());
        let tls_key = args
            .tls_key()
            .map(PathBuf::from)
            .or_else(|| config.server.tls.key.clone());

        validate_tls_pair(&tls_cert, &tls_key)?;
        if !bind.is_loopback() && tls_cert.is_none() {
            return Err(HttpBindError::RemoteBindRequiresTls);
        }

        Ok(Self {
            addr: SocketAddr::new(bind, port),
            tls_cert,
            tls_key,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HttpBindError {
    #[error("remote HTTP bind requires both --tls-cert and --tls-key")]
    RemoteBindRequiresTls,
    #[error("TLS configuration requires both cert and key")]
    TlsPairIncomplete,
    #[error("invalid governor configuration")]
    InvalidGovernorConfiguration,
}

pub async fn serve_http(
    args: &ServeArgs,
    config: &AppConfig,
    service: MemraService,
) -> anyhow::Result<()> {
    let options = HttpServeOptions::resolve(args, config)?;
    if options.tls_cert.is_some() {
        anyhow::bail!(
            "TLS listener support is not implemented yet; use loopback HTTP without TLS for now"
        );
    }

    let rmcp_config = streamable_http_config(options.addr.port());
    let session_manager = Arc::new(LocalSessionManager::default());
    let token_pool = service.token_pool();
    let service_factory = move || Ok(service.clone());
    let mcp_service = StreamableHttpService::new(service_factory, session_manager, rmcp_config);
    let idempotency_state = Arc::new(IdempotencyState {
        ledger: Arc::new(IdempotencyLedger::open(IdempotencyLedger::default_dir())?),
        project_id: args.project().unwrap_or("memra").to_string(),
    });
    let audit = AuditLogger::default();
    let auth_state = Arc::new(AuthState {
        authenticator: Arc::new(Authenticator::from_config(config)),
        audit: audit.clone(),
    });
    let session_state = Arc::new(SessionState::new(audit.clone()));
    let session_open_state = Arc::new(SessionOpenState {
        pool: token_pool,
        audit,
    });
    let app = build_http_app_with_session(
        mcp_service,
        idempotency_state,
        auth_state,
        session_state,
        session_open_state,
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    let listener = tokio::net::TcpListener::bind(options.addr).await?;
    tracing::info!(
        "Memra HTTP MCP listening on http://{}/mcp",
        options.addr
    );
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn allowed_hosts(port: u16) -> Vec<String> {
    vec![
        "127.0.0.1".to_string(),
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        format!("localhost:{port}"),
        "::1".to_string(),
        format!("[::1]:{port}"),
    ]
}

pub fn streamable_http_config(port: u16) -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts(port))
}

pub fn build_http_app(
    mcp_service: StreamableHttpService<MemraService, LocalSessionManager>,
    idempotency_state: Arc<IdempotencyState>,
    auth_state: Arc<AuthState>,
) -> Result<Router, HttpBindError> {
    let session_state = Arc::new(SessionState::new(auth_state.audit.clone()));
    // For build_http_app (used in tests without a real DB), create an in-memory
    // SessionOpenState so the /session/open route is wired but not functional.
    let session_open_state = Arc::new(SessionOpenState {
        pool: Arc::new(
            memra_core::storage::db::DbPool::open(std::path::Path::new(":memory:"))
                .expect("in-memory DbPool for session_open"),
        ),
        audit: auth_state.audit.clone(),
    });
    build_http_app_with_session(
        mcp_service,
        idempotency_state,
        auth_state,
        session_state,
        session_open_state,
    )
}

pub fn build_http_app_with_session(
    mcp_service: StreamableHttpService<MemraService, LocalSessionManager>,
    idempotency_state: Arc<IdempotencyState>,
    auth_state: Arc<AuthState>,
    session_state: Arc<SessionState>,
    session_open_state: Arc<SessionOpenState>,
) -> Result<Router, HttpBindError> {
    let mut governor_builder = GovernorConfigBuilder::default();
    governor_builder
        .per_second(60)
        .burst_size(GLOBAL_HTTP_RATE_LIMIT_PER_MINUTE);
    let governor_config = governor_builder
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .ok_or(HttpBindError::InvalidGovernorConfiguration)?;

    // Layer execution order in axum is LIFO (last added = first to run on request).
    // We want: auth -> session -> idempotency -> handler.
    // So layers are added in REVERSE: idempotency first, then session, then auth last.
    let mcp_router = Router::new()
        .route_service("/mcp", mcp_service)
        .layer(from_fn_with_state(
            idempotency_state,
            idempotency_middleware,
        ))
        .layer(from_fn_with_state(session_state, session_middleware))
        .layer(from_fn_with_state(Arc::clone(&auth_state), auth_middleware));

    // POST /session/open — behind auth_middleware, no session/idempotency layers needed.
    let session_open_router = Router::new()
        .route("/session/open", axum::routing::post(session_open_handler))
        .with_state(session_open_state)
        .layer(from_fn_with_state(auth_state, auth_middleware));

    Ok(Router::new()
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .merge(mcp_router)
        .merge(session_open_router)
        .layer(GovernorLayer::new(governor_config)))
}

fn validate_tls_pair(
    tls_cert: &Option<PathBuf>,
    tls_key: &Option<PathBuf>,
) -> Result<(), HttpBindError> {
    match (tls_cert, tls_key) {
        (Some(_), Some(_)) | (None, None) => Ok(()),
        _ => Err(HttpBindError::TlsPairIncomplete),
    }
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn metrics_handler() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        "ma_http_up 1\nma_http_rate_limit_per_minute 1000\n",
    )
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rmcp::transport::streamable_http_server::StreamableHttpService;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use tower::ServiceExt;

    use crate::cli::{Cli, Command};
    use crate::config::{AppConfig, AuthConfig, ServerConfig, TlsConfig};
    use crate::service::MemraService;
    use crate::transport::http::{HttpBindError, HttpServeOptions, allowed_hosts};

    fn parse_serve_args(args: [&str; 3]) -> Result<crate::cli::ServeArgs, String> {
        match Cli::try_parse_from(args)
            .map_err(|error| error.to_string())?
            .into_command()
        {
            Command::Serve(args) => Ok(args),
            other => Err(format!("expected serve command, got {other:?}")),
        }
    }

    fn config(bind: IpAddr, port: u16, tls_cert: Option<&str>, tls_key: Option<&str>) -> AppConfig {
        AppConfig {
            auth: AuthConfig::default(),
            server: ServerConfig {
                bind,
                port,
                tls: TlsConfig {
                    cert: tls_cert.map(PathBuf::from),
                    key: tls_key.map(PathBuf::from),
                },
            },
        }
    }

    #[test]
    fn http_options_use_config_when_cli_omits_bind_and_port() -> Result<(), String> {
        let args = parse_serve_args(["ma", "serve", "--http"])?;
        let config = config(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            8443,
            Some("/tmp/cert.pem"),
            Some("/tmp/key.pem"),
        );

        let options =
            HttpServeOptions::resolve(&args, &config).map_err(|error| error.to_string())?;

        assert_eq!(options.addr.ip().to_string(), "0.0.0.0");
        assert_eq!(options.addr.port(), 8443);
        assert_eq!(
            options
                .tls_cert
                .as_deref()
                .map(|path| path.display().to_string()),
            Some("/tmp/cert.pem".to_string())
        );
        assert_eq!(
            options
                .tls_key
                .as_deref()
                .map(|path| path.display().to_string()),
            Some("/tmp/key.pem".to_string())
        );
        Ok(())
    }

    #[test]
    fn cli_http_options_override_config() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "serve",
            "--http",
            "--bind",
            "127.0.0.1",
            "--port",
            "9000",
        ])
        .map_err(|error| error.to_string())?;
        let args = match parsed.into_command() {
            Command::Serve(args) => args,
            other => return Err(format!("expected serve command, got {other:?}")),
        };
        let config = config(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8443, None, None);

        let options =
            HttpServeOptions::resolve(&args, &config).map_err(|error| error.to_string())?;

        assert_eq!(options.addr.ip().to_string(), "127.0.0.1");
        assert_eq!(options.addr.port(), 9000);
        Ok(())
    }

    #[test]
    fn remote_bind_without_tls_is_rejected() -> Result<(), String> {
        let args = parse_serve_args(["ma", "serve", "--http"])?;
        let config = config(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8443, None, None);

        let error = match HttpServeOptions::resolve(&args, &config) {
            Ok(options) => return Err(format!("expected remote bind rejection, got {options:?}")),
            Err(error) => error,
        };

        assert!(matches!(error, HttpBindError::RemoteBindRequiresTls));
        Ok(())
    }

    #[test]
    fn rmcp_allowed_hosts_include_loopback_port_forms() {
        let hosts = allowed_hosts(7331);

        assert!(hosts.contains(&"127.0.0.1".to_string()));
        assert!(hosts.contains(&"127.0.0.1:7331".to_string()));
        assert!(hosts.contains(&"localhost".to_string()));
        assert!(hosts.contains(&"localhost:7331".to_string()));
        assert!(hosts.contains(&"::1".to_string()));
        assert!(hosts.contains(&"[::1]:7331".to_string()));
    }

    #[test]
    fn global_http_rate_limit_matches_decision() {
        assert_eq!(super::GLOBAL_HTTP_RATE_LIMIT_PER_MINUTE, 1000);
    }

    #[tokio::test]
    async fn rmcp_service_rejects_non_loopback_host_header() -> Result<(), String> {
        let service = test_streamable_service();
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "evil.example")
            .body(Body::from("{}"))
            .map_err(|error| error.to_string())?;

        let response = service.handle(request).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        Ok(())
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() -> Result<(), String> {
        let app = test_http_app()?;
        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .header("host", "127.0.0.1")
            .body(Body::empty())
            .map_err(|error| error.to_string())?;

        let response = app
            .oneshot(request)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_text() -> Result<(), String> {
        let app = test_http_app()?;
        let request = Request::builder()
            .method("GET")
            .uri("/metrics")
            .header("host", "127.0.0.1")
            .body(Body::empty())
            .map_err(|error| error.to_string())?;

        let response = app
            .oneshot(request)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn options_preflight_gets_405_without_cors_headers() -> Result<(), String> {
        let service = test_streamable_service();
        let request = Request::builder()
            .method("OPTIONS")
            .uri("/mcp")
            .header("host", "127.0.0.1:7331")
            .body(Body::empty())
            .map_err(|error| error.to_string())?;

        let response = service.handle(request).await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        Ok(())
    }

    fn test_streamable_service() -> StreamableHttpService<MemraService, LocalSessionManager>
    {
        let rmcp_config = super::streamable_http_config(7331);
        StreamableHttpService::new(
            || Ok(MemraService::stub()),
            Arc::new(LocalSessionManager::default()),
            rmcp_config,
        )
    }

    fn test_http_app() -> Result<Router, String> {
        let auth_state = Arc::new(crate::transport::auth::AuthState {
            authenticator: Arc::new(crate::transport::auth::Authenticator::from_config(
                &AppConfig::default(),
            )),
            audit: crate::audit::AuditLogger::default(),
        });
        super::build_http_app(
            test_streamable_service(),
            Arc::new(crate::transport::idempotency::IdempotencyState {
                ledger: Arc::new(
                    crate::transport::idempotency::IdempotencyLedger::open(
                        crate::transport::idempotency::IdempotencyLedger::default_dir(),
                    )
                    .map_err(|error| error.to_string())?,
                ),
                project_id: "test-project".to_string(),
            }),
            auth_state,
        )
        .map_err(|error| error.to_string())
    }
}

//! Authentication at the two HTTP MCP boundaries.
//!
//! Both mounts fail closed before an MCP request reaches the transport. The
//! agent surface requires the scoped credential minted for an active agent;
//! the operator surface requires a distinct human bearer supplied
//! out of band. Provider-backed agents are refused even when they discover
//! the mount or present their ordinary identity token: under one OS uid that
//! bearer can be copied across provider processes, so it is not sound
//! operator provenance.

use std::ffi::OsString;
use std::fmt;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use ciacola_core::{AgentIdentity, Ledger, TOKEN_HEADER};

pub const OPERATOR_TOKEN_FD_ENV: &str = "CIACOLA_OPERATOR_TOKEN_FD";
const UNSAFE_OPERATOR_TOKEN_ENV: &str = "CIACOLA_OPERATOR_TOKEN";
const CLIENT_BEARER_ENV: &str = "MCP_BEARER";
const CHALLENGE: &str = "Bearer realm=\"ciacola-operator\"";
const MIN_TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: u64 = 4096;

/// The human-only root bearer.
///
/// Deliberately has no `Display`, serialization, or accessor returning the
/// inner string. Even an accidental debug field remains redacted.
#[derive(Clone)]
pub struct HumanOperatorToken(Arc<str>);

impl fmt::Debug for HumanOperatorToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HumanOperatorToken([REDACTED])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorTokenError(&'static str);

impl fmt::Display for OperatorTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for OperatorTokenError {}

impl HumanOperatorToken {
    fn parse(raw: OsString) -> Result<Self, OperatorTokenError> {
        let raw = raw
            .into_string()
            .map_err(|_| OperatorTokenError("operator token must be valid UTF-8"))?;
        if raw.len() < MIN_TOKEN_BYTES {
            return Err(OperatorTokenError(
                "operator token must contain at least 32 bytes",
            ));
        }
        if !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
        {
            return Err(OperatorTokenError(
                "operator token must use only ASCII letters, digits, '-', '.', '_', or '~'",
            ));
        }
        Ok(Self(Arc::from(raw)))
    }

    fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
    }

    #[cfg(test)]
    fn for_test(value: &str) -> Self {
        Self::parse(OsString::from(value)).expect("valid test operator token")
    }
}

/// Consume the human bearer from an inherited descriptor before Tokio or any
/// provider process exists.
///
/// The secret itself must not be a startup environment value: on supported
/// desktop operating systems it can remain visible to same-user process
/// inspection even after `unsetenv`. Only the descriptor number is inherited.
/// The descriptor is read and closed here, before an agent can exist.
pub fn take_from_environment() -> Result<Option<HumanOperatorToken>, OperatorTokenError> {
    let descriptor = std::env::var_os(OPERATOR_TOKEN_FD_ENV);
    let unsafe_inline = std::env::var_os(UNSAFE_OPERATOR_TOKEN_ENV);
    let ambient_client_bearer = std::env::var_os(CLIENT_BEARER_ENV);

    // SAFETY: the binary calls this from plain synchronous `main`, before it
    // constructs the Tokio runtime or starts any other application thread.
    unsafe {
        std::env::remove_var(OPERATOR_TOKEN_FD_ENV);
        std::env::remove_var(UNSAFE_OPERATOR_TOKEN_ENV);
        // `MCP_BEARER` belongs to an HTTP client, never to the server or a
        // provider child. Drop an accidentally inherited copy at the same
        // pre-thread boundary.
        std::env::remove_var(CLIENT_BEARER_ENV);
    }

    if unsafe_inline.is_some() {
        return Err(OperatorTokenError(
            "CIACOLA_OPERATOR_TOKEN is unsafe at process startup; pass the secret through CIACOLA_OPERATOR_TOKEN_FD",
        ));
    }
    if ambient_client_bearer.is_some() {
        return Err(OperatorTokenError(
            "MCP_BEARER belongs to the HTTP client, not the server; refusing to leave it in the server's startup environment",
        ));
    }

    descriptor
        .map(read_token_descriptor)
        .transpose()?
        .map(HumanOperatorToken::parse)
        .transpose()
}

#[cfg(unix)]
fn read_token_descriptor(raw: OsString) -> Result<OsString, OperatorTokenError> {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    let descriptor: i32 = raw
        .into_string()
        .map_err(|_| OperatorTokenError("CIACOLA_OPERATOR_TOKEN_FD must be an integer"))?
        .parse()
        .map_err(|_| OperatorTokenError("CIACOLA_OPERATOR_TOKEN_FD must be an integer"))?;
    if descriptor < 3 {
        return Err(OperatorTokenError(
            "CIACOLA_OPERATOR_TOKEN_FD must not replace stdin, stdout, or stderr",
        ));
    }

    // Validate and duplicate before constructing a File: the descriptor came
    // from process metadata, so it cannot be trusted to satisfy
    // `File::from_raw_fd`'s safety contract. CLOEXEC also keeps the duplicate
    // out of any accidental child before it is dropped below.
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(OperatorTokenError(
            "CIACOLA_OPERATOR_TOKEN_FD is not an open readable descriptor",
        ));
    }
    // SAFETY: `duplicate` was returned open by fcntl and is now uniquely
    // owned by File. Closing the caller-owned original prevents later child
    // processes from inheriting the secret source.
    unsafe {
        libc::close(descriptor);
    }
    let file = unsafe { std::fs::File::from_raw_fd(duplicate) };
    let mut bytes = Vec::new();
    file.take(MAX_TOKEN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| OperatorTokenError("could not read CIACOLA_OPERATOR_TOKEN_FD"))?;
    if bytes.len() as u64 > MAX_TOKEN_BYTES {
        return Err(OperatorTokenError("operator token is unexpectedly large"));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    let token = String::from_utf8(bytes)
        .map_err(|_| OperatorTokenError("operator token must be valid UTF-8"))?;
    Ok(OsString::from(token))
}

#[cfg(not(unix))]
fn read_token_descriptor(_raw: OsString) -> Result<OsString, OperatorTokenError> {
    Err(OperatorTokenError(
        "CIACOLA_OPERATOR_TOKEN_FD is not supported on this platform; use stdio operator access",
    ))
}

#[derive(Clone)]
pub struct OperatorHttpAuth {
    human: Option<HumanOperatorToken>,
}

impl OperatorHttpAuth {
    pub fn new(human: Option<HumanOperatorToken>) -> Self {
        Self { human }
    }
}

/// Authenticate every request on the complete agent MCP mount and attach the
/// exact identity derived from its scoped credential.
///
/// Missing, malformed, unknown, and retired credentials all receive the same
/// response. Keeping those cases indistinguishable avoids turning the
/// loopback listener into a token or agent-state oracle.
pub async fn require_agent(
    State(ledger): State<Ledger>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut credentials = request.headers().get_all(TOKEN_HEADER).iter();
    let token = match (credentials.next(), credentials.next()) {
        (Some(value), None) => match value.to_str() {
            Ok(token) if !token.is_empty() => token,
            _ => return agent_unauthorized(),
        },
        _ => return agent_unauthorized(),
    };

    match ledger.agent_id_by_token(token).await {
        Ok(Some(agent_id)) => {
            request.extensions_mut().insert(AgentIdentity(agent_id));
            next.run(request).await
        }
        Ok(None) => agent_unauthorized(),
        Err(error) => {
            tracing::error!(%error, "agent HTTP authentication lookup failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

fn agent_unauthorized() -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

/// Authenticate every request on the complete operator MCP mount.
pub async fn require_operator(
    State(auth): State<OperatorHttpAuth>,
    request: Request,
    next: Next,
) -> Response {
    let authorization_present = request.headers().contains_key(AUTHORIZATION);
    let bearer = bearer(request.headers().get(AUTHORIZATION));
    let agent_credential_present = request.headers().contains_key(TOKEN_HEADER);

    if authorization_present && agent_credential_present {
        return (StatusCode::BAD_REQUEST, "ambiguous operator credentials").into_response();
    }
    if agent_credential_present {
        return (
            StatusCode::FORBIDDEN,
            "provider-backed agents cannot use the operator HTTP surface",
        )
            .into_response();
    }

    if let Some(candidate) = bearer {
        return match auth.human.as_ref() {
            Some(expected) if expected.matches(candidate) => next.run(request).await,
            _ => unauthorized(),
        };
    }

    unauthorized()
}

fn bearer(value: Option<&HeaderValue>) -> Option<&str> {
    let value = value?.to_str().ok()?;
    let (scheme, credential) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || credential.is_empty()
        || credential.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(credential)
}

fn unauthorized() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static(CHALLENGE));
    response
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left = left.get(index).copied().unwrap_or_default();
        let right = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::os::fd::IntoRawFd;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::http::Request as HttpRequest;
    use axum::middleware;
    use axum::routing::any;
    use tower::ServiceExt;
    use tower_mcp::extract::{Extension as McpExtension, RawArgs};
    use tower_mcp::transport::HttpTransport;
    use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

    use super::*;

    const HUMAN: &str = "human-operator-token-0123456789abcdef";

    async fn reached(Extension(calls): Extension<Arc<AtomicUsize>>) -> Response {
        calls.fetch_add(1, Ordering::SeqCst);
        StatusCode::OK.into_response()
    }

    async fn reached_as_agent(
        Extension(calls): Extension<Arc<AtomicUsize>>,
        Extension(seen): Extension<Arc<Mutex<Option<String>>>>,
        Extension(identity): Extension<AgentIdentity>,
    ) -> Response {
        calls.fetch_add(1, Ordering::SeqCst);
        *seen.lock().expect("identity lock") = Some(identity.0);
        StatusCode::OK.into_response()
    }

    fn router(auth: OperatorHttpAuth, calls: Arc<AtomicUsize>) -> Router {
        Router::new()
            .route("/mcp-operator", any(reached))
            .layer(Extension(calls))
            .route_layer(middleware::from_fn_with_state(auth, require_operator))
    }

    fn request() -> axum::http::request::Builder {
        HttpRequest::builder().method("POST").uri("/mcp-operator")
    }

    async fn ledger() -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        Ledger::setup(pool).await.expect("ledger")
    }

    fn agent_router(
        ledger: Ledger,
        calls: Arc<AtomicUsize>,
        seen: Arc<Mutex<Option<String>>>,
    ) -> Router {
        Router::new()
            .route("/mcp", any(reached_as_agent))
            .layer(Extension(calls))
            .layer(Extension(seen))
            .route_layer(middleware::from_fn_with_state(ledger, require_agent))
    }

    fn agent_request() -> axum::http::request::Builder {
        HttpRequest::builder().method("POST").uri("/mcp")
    }

    fn authenticated_mcp_router(ledger: Ledger) -> Router {
        let whoami = ToolBuilder::new("whoami")
            .extractor_handler(
                (),
                |McpExtension(identity): McpExtension<AgentIdentity>,
                 RawArgs(_): RawArgs| async move { Ok(CallToolResult::text(identity.0)) },
            )
            .build();
        HttpTransport::new(
            McpRouter::new()
                .server_info("ciacola-auth-test", "1.0.0")
                .tool(whoami),
        )
        .bridge_extension::<AgentIdentity>()
        .into_router_at("/mcp")
        .layer(middleware::from_fn_with_state(ledger, require_agent))
    }

    fn initialize_request(token: Option<&str>) -> axum::http::Request<Body> {
        let mut request = HttpRequest::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "127.0.0.1")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(token) = token {
            request = request.header(TOKEN_HEADER, token);
        }
        request
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "ciacola-auth-test", "version": "1" }
                    }
                })
                .to_string(),
            ))
            .expect("initialize request")
    }

    #[test]
    fn root_token_validation_and_debug_never_disclose_the_value() {
        let token = HumanOperatorToken::for_test(HUMAN);
        assert!(token.matches(HUMAN));
        assert!(!token.matches("human-operator-token-0123456789abcdeg"));
        assert_eq!(format!("{token:?}"), "HumanOperatorToken([REDACTED])");
        assert!(HumanOperatorToken::parse(OsString::from("short")).is_err());
        assert!(
            HumanOperatorToken::parse(OsString::from("human operator token 0123456789abcdef"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn agent_mount_rejects_missing_malformed_unknown_and_retired_credentials() {
        let ledger = ledger().await;
        let retired_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("retired", "system"), None)
            .await
            .expect("create retired agent");
        let retired_token = ledger
            .token_of(&retired_id)
            .await
            .expect("retired token query")
            .expect("retired token");
        assert!(ledger.retire_agent(&retired_id).await.expect("retire"));

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(None));
        let service = agent_router(ledger, calls.clone(), seen);

        let missing = service
            .clone()
            .oneshot(agent_request().body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let malformed = service
            .clone()
            .oneshot(
                agent_request()
                    .header(TOKEN_HEADER, "")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(malformed.status(), StatusCode::UNAUTHORIZED);

        let unknown = service
            .clone()
            .oneshot(
                agent_request()
                    .header(TOKEN_HEADER, "unknown-agent-credential")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);

        let retired = service
            .oneshot(
                agent_request()
                    .header(TOKEN_HEADER, retired_token)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(retired.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn agent_mount_dispatches_with_the_exact_authenticated_identity() {
        let ledger = ledger().await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("active", "system"), None)
            .await
            .expect("create active agent");
        let token = ledger
            .token_of(&agent_id)
            .await
            .expect("active token query")
            .expect("active token");

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(None));
        let response = agent_router(ledger, calls.clone(), seen.clone())
            .oneshot(
                agent_request()
                    .header(TOKEN_HEADER, token)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            seen.lock().expect("identity lock").as_deref(),
            Some(agent_id.as_str())
        );
    }

    #[tokio::test]
    async fn the_transport_health_endpoint_is_agent_authenticated_too() {
        let ledger = ledger().await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("active", "system"), None)
            .await
            .expect("create active agent");
        let token = ledger
            .token_of(&agent_id)
            .await
            .expect("token query")
            .expect("token");
        let service = authenticated_mcp_router(ledger);

        let anonymous = service
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/mcp/health")
                    .body(Body::empty())
                    .expect("health request"),
            )
            .await
            .expect("health response");
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let authenticated = service
            .oneshot(
                HttpRequest::builder()
                    .uri("/mcp/health")
                    .header(TOKEN_HEADER, token)
                    .body(Body::empty())
                    .expect("health request"),
            )
            .await
            .expect("health response");
        assert_eq!(authenticated.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_real_mcp_mount_authenticates_initialize_every_session_request_and_identity() {
        let ledger = ledger().await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("active", "system"), None)
            .await
            .expect("create active agent");
        let token = ledger
            .token_of(&agent_id)
            .await
            .expect("token query")
            .expect("token");
        let service = authenticated_mcp_router(ledger);

        let anonymous_initialize = service
            .clone()
            .oneshot(initialize_request(None))
            .await
            .expect("anonymous initialize response");
        assert_eq!(anonymous_initialize.status(), StatusCode::UNAUTHORIZED);

        let initialized = service
            .clone()
            .oneshot(initialize_request(Some(&token)))
            .await
            .expect("authenticated initialize response");
        assert_eq!(initialized.status(), StatusCode::OK);
        let session_id = initialized
            .headers()
            .get("mcp-session-id")
            .expect("session id")
            .to_str()
            .expect("session id text")
            .to_string();

        let initialized_notification = service
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("host", "127.0.0.1")
                    .header("content-type", "application/json")
                    .header("mcp-session-id", &session_id)
                    .header(TOKEN_HEADER, &token)
                    .body(Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/initialized"
                        })
                        .to_string(),
                    ))
                    .expect("initialized notification"),
            )
            .await
            .expect("initialized response");
        assert_eq!(initialized_notification.status(), StatusCode::ACCEPTED);

        let whoami = service
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("host", "127.0.0.1")
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .header("mcp-session-id", &session_id)
                    .header(TOKEN_HEADER, &token)
                    .body(Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "tools/call",
                            "params": { "name": "whoami", "arguments": {} }
                        })
                        .to_string(),
                    ))
                    .expect("whoami request"),
            )
            .await
            .expect("whoami response");
        assert_eq!(whoami.status(), StatusCode::OK);
        let body = axum::body::to_bytes(whoami.into_body(), usize::MAX)
            .await
            .expect("whoami body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("whoami JSON");
        assert_eq!(body["result"]["content"][0]["text"], agent_id);

        let anonymous_existing_session = service
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("host", "127.0.0.1")
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .header("mcp-session-id", session_id)
                    .body(Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 3,
                            "method": "tools/list",
                            "params": {}
                        })
                        .to_string(),
                    ))
                    .expect("tools/list request"),
            )
            .await
            .expect("anonymous session response");
        assert_eq!(
            anonymous_existing_session.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_token_is_consumed_from_a_dedicated_descriptor() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        writer.write_all(HUMAN.as_bytes()).expect("write token");
        writer.write_all(b"\n").expect("write newline");
        drop(writer);

        let raw = read_token_descriptor(OsString::from(reader.into_raw_fd().to_string()))
            .expect("read descriptor");
        let token = HumanOperatorToken::parse(raw).expect("parse token");
        assert!(token.matches(HUMAN));
    }

    #[tokio::test]
    async fn missing_and_invalid_human_credentials_fail_before_dispatch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = router(
            OperatorHttpAuth::new(Some(HumanOperatorToken::for_test(HUMAN))),
            calls.clone(),
        );

        let missing = service
            .clone()
            .oneshot(request().body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            missing.headers().get(WWW_AUTHENTICATE),
            Some(&HeaderValue::from_static(CHALLENGE))
        );

        let invalid = service
            .oneshot(
                request()
                    .header(AUTHORIZATION, "Bearer definitely-not-the-right-token-value")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_human_bearer_succeeds_without_an_agent_identity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let response = router(
            OperatorHttpAuth::new(Some(HumanOperatorToken::for_test(HUMAN))),
            calls.clone(),
        )
        .oneshot(
            request()
                .header(AUTHORIZATION, format!("Bearer {HUMAN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn every_agent_credential_is_refused_before_dispatch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let response = router(OperatorHttpAuth::new(None), calls.clone())
            .oneshot(
                request()
                    .header(TOKEN_HEADER, "real-or-stolen-agent-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn human_and_agent_tokens_cannot_be_substituted_or_combined() {
        let agent_token = "agent-token-that-is-long-enough-but-not-human";
        let calls = Arc::new(AtomicUsize::new(0));
        let service = router(
            OperatorHttpAuth::new(Some(HumanOperatorToken::for_test(HUMAN))),
            calls.clone(),
        );

        let agent_as_bearer = service
            .clone()
            .oneshot(
                request()
                    .header(AUTHORIZATION, format!("Bearer {agent_token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(agent_as_bearer.status(), StatusCode::UNAUTHORIZED);

        let human_as_agent = service
            .clone()
            .oneshot(
                request()
                    .header(TOKEN_HEADER, HUMAN)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(human_as_agent.status(), StatusCode::FORBIDDEN);

        let ambiguous = service
            .oneshot(
                request()
                    .header(AUTHORIZATION, format!("Bearer {HUMAN}"))
                    .header(TOKEN_HEADER, agent_token)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(ambiguous.status(), StatusCode::BAD_REQUEST);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

//! WebDAV server mode. Serves an Internxt Drive (or active workspace) over
//! WebDAV so it can be mounted by any WebDAV client. Mirrors the observable
//! behaviour of og/cli's `webdav` server (handlers, status codes, XML shape),
//! but runs in the foreground as a normal command instead of as a pm2 service.
//!
//! Plain HTTP by default. HTTPS (self-signed or custom cert) is available when
//! built with the `webdav-tls` feature.

mod cache;
mod etag;
mod handlers;
mod resource;
mod xml;

#[cfg(feature = "webdav-tls")]
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::{engine::general_purpose::STANDARD as B64, Engine};

use internxt_core::config;
use internxt_core::models::Credentials;
use crate::serve::creds::SharedCreds;

/// Transport for the WebDAV listener.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Https,
}

/// Runtime configuration for a `webdav` server run (built from CLI flags).
pub struct WebdavConfig {
    /// Bind + display host (e.g. `127.0.0.1`, `0.0.0.0`).
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
    /// Server request timeout in minutes (long transfers). 0 = no timeout.
    /// Reserved: accepted for parity/forward-compat, not yet wired to a timeout layer.
    #[allow(dead_code)]
    pub timeout_minutes: u64,
    /// Auto-create missing parent folders on PUT / MKCOL.
    pub create_full_path: bool,
    /// Optional HTTP Basic auth (username, password) required from clients.
    pub custom_auth: Option<(String, String)>,
    /// Delete files permanently instead of moving them to trash.
    pub delete_permanently: bool,
    /// Serve read-only: reject every mutating method (PUT / MKCOL / DELETE /
    /// MOVE / COPY / PROPPATCH) with 403.
    pub read_only: bool,
    /// Spool each PUT body to a temp file before uploading (instead of streaming
    /// the live client body straight to network storage). More robust under
    /// concurrent/slow clients; costs temp disk + a little latency.
    pub spool: bool,
    /// Directory for `--spool` temp files. `None` = system temp dir.
    pub spool_dir: Option<PathBuf>,
    /// TLS certificate (PEM). When omitted with `Https`, a self-signed cert is used.
    pub cert: Option<PathBuf>,
    /// TLS private key (PEM). Required alongside `cert`.
    pub key: Option<PathBuf>,
}

/// Shared server state. `creds` is behind a lock so a background task can swap
/// in a refreshed token without restarting the server; each request takes a
/// cheap snapshot (`ctx.creds()`) at its start.
pub struct Ctx {
    creds: Arc<SharedCreds>,
    pub config: WebdavConfig,
    pub root_folder: String,
    pub root_updated_at: String,
    /// Short-TTL cache of folder listings, to collapse the repeated tree walks
    /// that path resolution does under a burst of requests. Shared with the
    /// other serve backends running in the same process.
    pub cache: Arc<cache::FolderCache>,
    /// Limits concurrent PUT network transfers when set (`--max-concurrent-uploads`).
    /// `None` = unlimited. Shared (process-wide cap) across serve backends.
    pub upload_sem: Option<Arc<tokio::sync::Semaphore>>,
    /// Per-file upload size cap, enforced on PUT.
    pub upload_limit: crate::upload_limit::UploadLimit,
}

impl Ctx {
    /// Current credentials snapshot (a cheap `Arc` clone). Handlers hold this
    /// for the duration of one request so a mid-request refresh is consistent.
    pub fn creds(&self) -> Arc<Credentials> {
        self.creds.get()
    }

    /// Acquire an upload slot, blocking while the concurrency limit is saturated.
    /// Returns `None` when uploads are unlimited. The returned permit must be
    /// held for the duration of the network transfer.
    pub async fn acquire_upload(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        match &self.upload_sem {
            Some(sem) => Some(sem.acquire().await.expect("upload semaphore never closed")),
            None => None,
        }
    }
}

/// An error carrying an HTTP status; rendered as a WebDAV `<D:error>` document.
pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        AppError { status, message: message.into() }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, msg)
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
    pub fn payload_too_large(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, msg)
    }
}

/// Map an upstream Drive status onto the status this server answers with.
///
/// Mirrors og's `errors.middleware.ts`, which forwards the SDK error's
/// `statusCode`/`status` and defaults to 500 — with two guards og doesn't need:
///
/// * **Only error statuses are forwarded.** A 1xx/2xx/3xx reaching here would
///   mean an engine call failed *without* an error status (or with one `http`
///   cannot represent), and answering a WebDAV client "204 No Content" for a
///   failed PROPFIND is worse than an honest 500.
/// * **Upstream 401/403 both become 403.** This server has its own auth layer
///   (see [`check_auth`]), and a bare 401 is that layer's language: clients
///   answer it by re-prompting for the *WebDAV* password and retrying. A
///   Drive-side 401 means the stored Drive session is stale — a password the
///   user types into their WebDAV client cannot fix it, so re-prompting would
///   just loop. 403 says "authenticated here, refused there", which is exactly
///   what happened, and it is a status WebDAV clients surface instead of
///   swallowing. (The fix is `ixr login` on the server side.)
///
/// Takes the plain number rather than core's `reqwest::StatusCode`: `reqwest`
/// is not a direct dependency here, and `axum`'s `http` need not be the same
/// crate version core resolved.
fn client_status(upstream: u16) -> StatusCode {
    match StatusCode::from_u16(upstream) {
        Ok(StatusCode::UNAUTHORIZED) => StatusCode::FORBIDDEN,
        Ok(s) if s.is_client_error() || s.is_server_error() => s,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Turn an engine error into a WebDAV response.
///
/// A typed [`internxt_core::ApiError`] carries the upstream Drive status and
/// the server's own explanation, and both reach the client: the status via
/// [`client_status`], the explanation appended in brackets the way og's
/// `errors.middleware.ts` does (`"{message} [{detail}]"`). Where og reads the
/// detail off the SDK error's `data.message` (string, or array joined with
/// `", "`), core has already done that extraction in `ApiError::detail()`.
///
/// Everything else — network, IO, crypto, a body that no longer deserializes —
/// is untyped and stays a 500 with the full `anyhow` chain, as before.
impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        let Some(api) = e.downcast_ref::<internxt_core::ApiError>() else {
            return AppError::internal(format!("{e:#}"));
        };
        let message = match api.detail() {
            // The server explained itself: show the explanation rather than the
            // raw JSON body it was extracted from.
            Some(detail) => format!("{} failed: HTTP {} [{detail}]", api.context(), api.status()),
            // Nothing quotable in the body (empty, non-JSON, no `message`) —
            // keep the engine's full message, raw body included. It is all the
            // diagnostic there is.
            None => format!("{e:#}"),
        };
        AppError::new(client_status(api.status_code()), message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        crate::serve::log::warn(&format!(
            "[ERROR] [{}] {}",
            self.status.as_u16(),
            self.message
        ));
        let body = xml::error(&self.message);
        Response::builder()
            .status(self.status)
            .header("Content-Type", "application/xml; charset=\"utf-8\"")
            .body(Body::from(body))
            .unwrap()
    }
}

/// Per-request trace (`[METHOD] path`). Verbose-only: printed just when
/// `--verbose` is set, so a busy server doesn't spam stderr by default.
pub(crate) fn log(msg: &str) {
    crate::serve::log::trace(msg);
}

/// Whether the wire-level header dump is enabled: the shared `--verbose` flag.
fn debug_enabled() -> bool {
    crate::serve::log::verbose()
}

/// Run the WebDAV backend until `shutdown` resolves. Credentials, the folder
/// cache and the upload limiter are supplied by the `serve` orchestrator and
/// shared with any sibling backends.
pub async fn serve(
    shared: Arc<crate::serve::run::Shared>,
    config: WebdavConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // Validate custom-auth completeness up front.
    if let Some((u, p)) = &config.custom_auth {
        if u.trim().is_empty() || p.trim().is_empty() {
            return Err(anyhow!(
                "custom authentication requires both --username and --password"
            ));
        }
    }

    // Ensure the spool directory exists and is writable before we start serving.
    if let Some(dir) = &config.spool_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("spool directory {} is not usable: {e}", dir.display()))?;
    }

    let root_folder = shared.root_folder.clone();
    let root_updated_at = shared.root_updated_at.clone();

    let scheme = match config.protocol {
        Protocol::Http => "http",
        Protocol::Https => "https",
    };
    let banner = format!(
        "Internxt {} WebDAV server listening at {scheme}://{}:{}{}{}",
        config::client_version(),
        config.host,
        config.port,
        if config.custom_auth.is_some() {
            " (with custom authentication)"
        } else {
            ""
        },
        if config.read_only { " (read-only)" } else { "" },
    );
    let host = config.host.clone();
    let port = config.port;
    let protocol = config.protocol;
    let cert = config.cert.clone();
    let key = config.key.clone();

    let ctx = Arc::new(Ctx {
        creds: shared.creds.clone(),
        config,
        root_folder,
        root_updated_at,
        cache: shared.cache.clone(),
        upload_sem: shared.upload_sem.clone(),
        upload_limit: shared.upload_limit,
    });

    let app = Router::new().fallback(dispatch).with_state(ctx);

    crate::output::status(&banner);

    match protocol {
        Protocol::Http => {
            let listener = tokio::net::TcpListener::bind((host.as_str(), port))
                .await
                .map_err(|e| anyhow!("failed to bind {host}:{port}: {e}"))?;
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await?;
        }
        Protocol::Https => {
            tokio::select! {
                res = serve_https(&host, port, cert, key, app) => res?,
                _ = shutdown => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Root request dispatcher: authenticates then routes by HTTP method (including
/// the WebDAV verbs axum's normal routing can't match).
async fn dispatch(State(ctx): State<Arc<Ctx>>, req: Request) -> Response {
    if let Some(resp) = check_auth(&ctx, &req) {
        return resp;
    }

    let method = req.method().clone();
    log(&format!("[{}] {}", method.as_str(), req.uri().path()));

    // Opt-in wire-level debug: dump the request line + all headers so we can see
    // exactly what a client (e.g. WinSCP) sends. Enable with --verbose.
    if debug_enabled() {
        eprintln!("  >> {} {} {:?}", method.as_str(), req.uri(), req.version());
        for (name, value) in req.headers() {
            eprintln!("  >> {}: {}", name, value.to_str().unwrap_or("<binary>"));
        }
    }

    // Read-only mode: reject every mutating verb up front (draining the body so
    // keep-alive survives), before any handler touches the Drive.
    if ctx.config.read_only
        && matches!(
            method.as_str(),
            "PUT" | "MKCOL" | "DELETE" | "MOVE" | "COPY" | "PROPPATCH"
        )
    {
        let resp = handlers::unsupported(
            req,
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.",
        )
        .await
        .unwrap_or_else(|e| e.into_response());
        return resp;
    }

    let result = match method.as_str() {
        "OPTIONS" => handlers::options(req),
        "PROPFIND" => handlers::propfind(&ctx, req).await,
        "GET" => handlers::get(&ctx, req, false).await,
        "HEAD" => handlers::get(&ctx, req, true).await,
        "PUT" => handlers::put(&ctx, req).await,
        "MKCOL" => handlers::mkcol(&ctx, req).await,
        "DELETE" => handlers::delete(&ctx, req).await,
        "MOVE" => handlers::mv(&ctx, req).await,
        "LOCK" => handlers::lock(req).await,
        "UNLOCK" => {
            // No body expected, but drain defensively to preserve keep-alive.
            handlers::drain_body(req.into_body()).await;
            Ok(status_response(StatusCode::NO_CONTENT))
        }
        "COPY" => {
            handlers::unsupported(req, StatusCode::NOT_IMPLEMENTED, "COPY is not implemented yet.")
                .await
        }
        "PROPPATCH" => {
            handlers::unsupported(
                req,
                StatusCode::NOT_IMPLEMENTED,
                "PROPPATCH is not implemented yet.",
            )
            .await
        }
        other => {
            let msg = format!("Method {other} not allowed");
            handlers::unsupported(req, StatusCode::METHOD_NOT_ALLOWED, msg).await
        }
    };

    let resp = result.unwrap_or_else(|e| e.into_response());
    if debug_enabled() {
        eprintln!("  << {} response {}", method.as_str(), resp.status().as_u16());
        for (name, value) in resp.headers() {
            eprintln!("  << {}: {}", name, value.to_str().unwrap_or("<binary>"));
        }
    }
    resp
}

/// Enforce optional HTTP Basic auth. Returns `Some(401)` when the request is
/// rejected, `None` when it may proceed.
fn check_auth(ctx: &Ctx, req: &Request) -> Option<Response> {
    let (want_user, want_pass) = ctx.config.custom_auth.as_ref()?;

    let unauthorized = |msg: &str| -> Option<Response> {
        let mut resp = AppError::new(StatusCode::UNAUTHORIZED, msg).into_response();
        resp.headers_mut().insert(
            "WWW-Authenticate",
            HeaderValue::from_static("Basic realm=\"Internxt WebDAV\""),
        );
        Some(resp)
    };

    let header = match req.headers().get("authorization").and_then(|v| v.to_str().ok()) {
        Some(h) => h,
        None => return unauthorized("Missing Authorization header."),
    };
    let b64 = match header.strip_prefix("Basic ") {
        Some(b) => b,
        None => return unauthorized("Only Basic authentication is supported."),
    };
    let decoded = match B64.decode(b64).ok().and_then(|b| String::from_utf8(b).ok()) {
        Some(d) => d,
        None => return unauthorized("Invalid authentication credentials format."),
    };
    let (user, pass) = match decoded.split_once(':') {
        Some(pair) => pair,
        None => return unauthorized("Invalid authentication credentials format."),
    };
    if user == want_user && pass == want_pass {
        None
    } else {
        unauthorized("Authentication failed. Check your WebDAV custom credentials.")
    }
}

/// An empty-body response with just a status code.
pub(crate) fn status_response(status: StatusCode) -> Response {
    Response::builder().status(status).body(Body::empty()).unwrap()
}

// ---- HTTPS ----

#[cfg(feature = "webdav-tls")]
async fn serve_https(
    host: &str,
    port: u16,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    app: Router,
) -> Result<()> {
    use axum_server::tls_rustls::RustlsConfig;

    let tls = match (cert, key) {
        (Some(cert), Some(key)) => RustlsConfig::from_pem_file(cert, key)
            .await
            .map_err(|e| anyhow!("failed to load TLS cert/key: {e}"))?,
        (None, None) => {
            crate::output::status("No --cert/--key given; generating a self-signed certificate.");
            let (cert_pem, key_pem) = self_signed_cert(host)?;
            RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
                .await
                .map_err(|e| anyhow!("failed to build self-signed TLS config: {e}"))?
        }
        _ => {
            return Err(anyhow!("--cert and --key must be provided together"));
        }
    };

    let addr: SocketAddr = resolve_addr(host, port)?;
    axum_server::bind_rustls(addr, tls)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}

#[cfg(not(feature = "webdav-tls"))]
async fn serve_https(
    _host: &str,
    _port: u16,
    _cert: Option<PathBuf>,
    _key: Option<PathBuf>,
    _app: Router,
) -> Result<()> {
    Err(anyhow!(
        "HTTPS is not available: this binary was built without the `webdav-tls` feature"
    ))
}

#[cfg(feature = "webdav-tls")]
fn self_signed_cert(host: &str) -> Result<(String, String)> {
    let mut sans = vec!["localhost".to_string()];
    if !host.is_empty() && host != "0.0.0.0" && !sans.contains(&host.to_string()) {
        sans.push(host.to_string());
    }
    let cert = rcgen::generate_simple_self_signed(sans)
        .map_err(|e| anyhow!("failed to generate self-signed cert: {e}"))?;
    Ok((cert.cert.pem(), cert.signing_key.serialize_pem()))
}

#[cfg(feature = "webdav-tls")]
fn resolve_addr(host: &str, port: u16) -> Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .map_err(|e| anyhow!("failed to resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| anyhow!("no address resolved for {host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use internxt_core::ApiError;

    /// An engine error carrying an upstream status and body, as core raises it.
    fn api_err(status: u16, body: &str) -> anyhow::Error {
        ApiError::new(
            "getFolderMeta",
            reqwest::StatusCode::from_u16(status).unwrap(),
            body,
        )
        .into()
    }

    fn mapped(status: u16, body: &str) -> AppError {
        AppError::from(api_err(status, body))
    }

    #[test]
    fn upstream_error_status_reaches_the_client() {
        // og forwards the SDK error's status instead of collapsing to 500.
        assert_eq!(mapped(404, "{}").status, StatusCode::NOT_FOUND);
        assert_eq!(mapped(507, "{}").status, StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(mapped(402, "{}").status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(mapped(429, "{}").status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(mapped(500, "{}").status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn an_untyped_error_is_still_a_500() {
        let e = AppError::from(anyhow!("connection reset by peer"));
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.message, "connection reset by peer");
        // Text that merely looks like an API error is not one.
        let looks_like = AppError::from(anyhow!("getFolderMeta failed: HTTP 404 Not Found: {{}}"));
        assert_eq!(looks_like.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn drive_auth_failures_become_403_not_401() {
        // 401 must not reach the client as 401: that is this server's own auth
        // layer's language and would make clients re-prompt for the WebDAV
        // password over a stale *Drive* session. 403 for both.
        assert_eq!(mapped(401, "{}").status, StatusCode::FORBIDDEN);
        assert_eq!(mapped(403, "{}").status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_non_error_status_falls_back_to_500() {
        // Nothing should produce these, but a 2xx/3xx answer rendered as a
        // failure would be a nonsense response — 500 is the honest one.
        assert_eq!(mapped(200, "{}").status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(mapped(204, "{}").status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(mapped(302, "{}").status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(mapped(100, "{}").status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn the_server_detail_is_appended_in_brackets() {
        // og: `message += " [" + detail + "]"`, detail = the body's `message`.
        let e = mapped(404, r#"{"statusCode":404,"message":"Folder not found"}"#);
        assert_eq!(e.message, "getFolderMeta failed: HTTP 404 Not Found [Folder not found]");
        // An array `message` (NestJS validation) is joined by core.
        let e = mapped(400, r#"{"message":["name must be a string","name is required"]}"#);
        assert_eq!(
            e.message,
            "getFolderMeta failed: HTTP 400 Bad Request [name must be a string, name is required]"
        );
    }

    #[test]
    fn without_a_detail_the_raw_message_is_kept() {
        // No `message` field, non-JSON, and empty bodies all leave nothing to
        // quote — the engine's own message (raw body included) stands.
        let e = mapped(500, r#"{"statusCode":500,"error":"Internal Server Error"}"#);
        assert_eq!(
            e.message,
            r#"getFolderMeta failed: HTTP 500 Internal Server Error: {"statusCode":500,"error":"Internal Server Error"}"#
        );
        let e = mapped(403, "<Error><Code>AccessDenied</Code></Error>");
        assert_eq!(
            e.message,
            "getFolderMeta failed: HTTP 403 Forbidden: <Error><Code>AccessDenied</Code></Error>"
        );
        assert_eq!(mapped(404, "").message, "getFolderMeta failed: HTTP 404 Not Found");
    }

    #[test]
    fn the_mapped_status_is_what_the_response_carries() {
        let resp = mapped(507, r#"{"message":"Not enough storage"}"#).into_response();
        assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE);
    }
}

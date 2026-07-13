//! Web-based (universal-link) SSO login.
//!
//! Starts a temporary local HTTP server (axum) that receives the login callback
//! from the Internxt web app, then builds credentials from the delivered
//! mnemonic / token / private key. Mirrors og/cli universal-link.service.ts.
//! Compiled only with the `sso` feature.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Redirect},
    routing::get,
    Router,
};

use crate::auth;
use crate::config;
use crate::models::Credentials;
use crate::output;

/// Raw callback query params. Each value is base64 of the cleartext.
#[derive(serde::Deserialize)]
struct CallbackParams {
    mnemonic: Option<String>,
    #[serde(rename = "newToken")]
    new_token: Option<String>,
    #[serde(rename = "privateKey")]
    private_key: Option<String>,
}

/// Decoded (cleartext) callback values.
struct SsoSession {
    mnemonic: String,
    token: String,
    private_key_pem: String,
}

type ResultSender = Arc<Mutex<Option<oneshot::Sender<Result<SsoSession>>>>>;

/// Runs the full SSO login: spins up the local callback server, opens the
/// browser at the web login page, waits for the callback, and returns the
/// resulting credentials.
///
/// `host` is the address the browser will use to reach this machine (defaults
/// to 127.0.0.1; set it when logging in from a browser on another device).
/// `port` fixes the callback port; when omitted a random free port is used.
pub async fn login(host: Option<&str>, port: Option<u16>) -> Result<Credentials> {
    let host = host.unwrap_or("127.0.0.1");

    // Bind on all interfaces so a browser on another device can reach the
    // callback; `host` is only used to build the URL the browser calls back to.
    let bind: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port.unwrap_or(0)));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow!("failed to start local login server on {bind}: {e}"))?;
    let actual_port = listener.local_addr()?.port();

    let (tx, rx) = oneshot::channel::<Result<SsoSession>>();
    let sender: ResultSender = Arc::new(Mutex::new(Some(tx)));

    // Graceful-shutdown trigger fired once the callback has been handled.
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let app = Router::new()
        .route("/callback", get(callback))
        .with_state(sender);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });
    let server_handle = tokio::spawn(async move {
        let _ = server.await;
    });

    // Build and open the web login URL.
    let redirect_uri = B64.encode(format!("http://{host}:{actual_port}/callback"));
    let login_url = build_login_url(&redirect_uri);

    output::status("Opening browser for login...");
    output::status("If the browser doesn't open automatically, visit:");
    output::emit(
        &login_url,
        serde_json::json!({ "loginUrl": login_url }),
    );
    if open::that(&login_url).is_err() {
        output::status("warning: could not open browser automatically.");
    }
    output::status("Waiting for authentication...");

    // Wait for the callback handler to deliver a result, then stop the server.
    let result = rx
        .await
        .map_err(|_| anyhow!("login server closed before receiving a response"))?;
    let _ = shutdown_tx.send(());
    let _ = server_handle.await;

    let session = result?;
    auth::build_sso_credentials(&session.mnemonic, &session.token, &session.private_key_pem).await
}

fn build_login_url(redirect_uri: &str) -> String {
    let base = config::drive_web_url();
    let enc = urlencode(redirect_uri);
    format!("{base}/login?universalLink=true&redirectUri={enc}")
}

/// Minimal percent-encoding for the base64 redirect URI (base64 may contain
/// `+`, `/`, `=`). Avoids pulling in a URL-encoding dependency.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Callback handler: decodes the delivered params, hands the result to the main
/// task, and redirects the browser to the web app's success/error page.
async fn callback(
    State(sender): State<ResultSender>,
    Query(params): Query<CallbackParams>,
) -> impl IntoResponse {
    let result = decode_session(params);
    let ok = result.is_ok();

    // Deliver the result to the waiting task (first callback wins).
    if let Some(tx) = sender.lock().unwrap().take() {
        let _ = tx.send(result);
    }

    let drive = config::drive_web_url();
    let target = if ok {
        format!("{drive}/auth-link-ok")
    } else {
        format!("{drive}/auth-link-error")
    };
    Redirect::to(&target)
}

fn decode_session(params: CallbackParams) -> Result<SsoSession> {
    let (mnemonic, token, private_key) = match (params.mnemonic, params.new_token, params.private_key)
    {
        (Some(m), Some(t), Some(p)) if !m.is_empty() && !t.is_empty() && !p.is_empty() => (m, t, p),
        _ => return Err(anyhow!("Login has failed, please try again")),
    };

    let decode = |v: &str, what: &str| -> Result<String> {
        let bytes = B64
            .decode(v)
            .map_err(|_| anyhow!("invalid base64 {what} in login callback"))?;
        String::from_utf8(bytes).map_err(|_| anyhow!("invalid utf-8 {what} in login callback"))
    };

    Ok(SsoSession {
        mnemonic: decode(&mnemonic, "mnemonic")?,
        token: decode(&token, "token")?,
        private_key_pem: decode(&private_key, "privateKey")?,
    })
}

//! Native SSO callback transport + front-end. Implements core's
//! [`internxt_core::sso::SsoCallbackServer`] with a temporary local axum HTTP
//! server, and drives the flow: prints the login URL, opens the browser, and
//! reports progress. All the axum / tokio-net bits live here (not in core) so
//! core stays portable.

use anyhow::{anyhow, Result};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Redirect},
    routing::get,
    Router,
};

use internxt_core::models::Credentials;
use internxt_core::sso::{self, SsoCallback, SsoCallbackServer};

/// Raw callback query params. Each value is base64 of the cleartext.
#[derive(serde::Deserialize)]
struct CallbackParams {
    mnemonic: Option<String>,
    #[serde(rename = "newToken")]
    new_token: Option<String>,
    #[serde(rename = "privateKey")]
    private_key: Option<String>,
}

type CallbackSender = Arc<Mutex<Option<oneshot::Sender<SsoCallback>>>>;

/// A local HTTP server (axum) that receives the SSO callback on `/callback`.
struct NativeCallbackServer {
    redirect_uri: String,
    rx: oneshot::Receiver<SsoCallback>,
    shutdown_tx: oneshot::Sender<()>,
    server_handle: tokio::task::JoinHandle<()>,
}

impl NativeCallbackServer {
    /// Binds the callback server. `host` is the address the browser uses to reach
    /// this machine (only used to build the redirect URI); the socket binds on all
    /// interfaces so a browser on another device can reach it. `port` fixes the
    /// callback port; a random free port is used when omitted.
    async fn bind(host: &str, port: Option<u16>) -> Result<Self> {
        let bind: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port.unwrap_or(0)));
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| anyhow!("failed to start local login server on {bind}: {e}"))?;
        let actual_port = listener.local_addr()?.port();

        let (tx, rx) = oneshot::channel::<SsoCallback>();
        let sender: CallbackSender = Arc::new(Mutex::new(Some(tx)));

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

        Ok(Self {
            redirect_uri: format!("http://{host}:{actual_port}/callback"),
            rx,
            shutdown_tx,
            server_handle,
        })
    }
}

impl SsoCallbackServer for NativeCallbackServer {
    fn redirect_uri(&self) -> String {
        self.redirect_uri.clone()
    }

    fn wait(self) -> impl Future<Output = Result<SsoCallback>> + Send {
        async move {
            let cb = self
                .rx
                .await
                .map_err(|_| anyhow!("login server closed before receiving a response"))?;
            let _ = self.shutdown_tx.send(());
            let _ = self.server_handle.await;
            Ok(cb)
        }
    }
}

/// Callback handler: hands the raw params to the waiting task and redirects the
/// browser to the web app's success/error page.
async fn callback(
    State(sender): State<CallbackSender>,
    Query(params): Query<CallbackParams>,
) -> impl IntoResponse {
    let cb = SsoCallback {
        mnemonic: params.mnemonic,
        new_token: params.new_token,
        private_key: params.private_key,
    };
    let ok = sso::callback_ok(&cb);

    // Deliver to the waiting task (first callback wins).
    if let Some(tx) = sender.lock().unwrap().take() {
        let _ = tx.send(cb);
    }

    let drive = internxt_core::config::drive_web_url();
    let target = if ok {
        format!("{drive}/auth-link-ok")
    } else {
        format!("{drive}/auth-link-error")
    };
    Redirect::to(&target)
}

/// Runs the web-based SSO login and returns credentials. Opens the browser at the
/// URL core hands back (and prints it as a fallback / for `--json`).
pub async fn login(host: Option<&str>, port: Option<u16>) -> Result<Credentials> {
    let host = host.unwrap_or("127.0.0.1");
    let server = NativeCallbackServer::bind(host, port).await?;
    sso::login(server, |url| {
        crate::output::status("Opening browser for login...");
        crate::output::status("If the browser doesn't open automatically, visit:");
        crate::output::emit(url, serde_json::json!({ "loginUrl": url }));
        if open::that(url).is_err() {
            crate::output::status("warning: could not open browser automatically.");
        }
        crate::output::status("Waiting for authentication...");
    })
    .await
}

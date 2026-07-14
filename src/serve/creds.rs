//! Shared, refreshable credentials holder for long-running serve backends.
//!
//! A `mount` / `serve` process can outlive its session token, so both backends
//! keep credentials behind a lock that a background task swaps in-place after a
//! periodic `get_auth_details` refresh. Each request/op takes a cheap `Arc`
//! snapshot (`get()`) at its start, so a mid-op refresh stays consistent.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::models::Credentials;

/// How often the background task re-checks / refreshes the session token. The
/// underlying `get_auth_details` only hits the network when the token is within
/// two days of expiry, so a frequent tick is cheap.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);

/// Credentials behind an `RwLock<Arc<…>>`: readers snapshot cheaply, the
/// refresh task swaps a new `Arc` in without disturbing in-flight snapshots.
pub struct SharedCreds(RwLock<Arc<Credentials>>);

impl SharedCreds {
    pub fn new(creds: Credentials) -> Self {
        SharedCreds(RwLock::new(Arc::new(creds)))
    }

    /// Current credentials snapshot (a cheap `Arc` clone). Hold it for the
    /// duration of one op so a mid-op refresh never changes the token underneath.
    pub fn get(&self) -> Arc<Credentials> {
        self.0.read().unwrap().clone()
    }

    /// Swap in refreshed credentials.
    pub fn set(&self, creds: Credentials) {
        *self.0.write().unwrap() = Arc::new(creds);
    }
}

/// Periodically refresh the session (and workspace) token so a long-lived
/// backend doesn't outlive its credentials. Best-effort: on failure the last
/// good snapshot is kept and a warning is logged.
pub fn spawn_refresh(shared: Arc<SharedCreds>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH_INTERVAL).await;
            match crate::auth::get_auth_details().await {
                Ok(creds) => shared.set(creds),
                Err(e) => eprintln!("[REFRESH] credential refresh failed: {e:#}"),
            }
        }
    });
}

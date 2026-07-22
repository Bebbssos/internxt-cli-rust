//! Wraps `NetworkApi` construction so `--no-timeout`/`IXR_NO_TIMEOUTS` can
//! disable its idle-read timeout without threading a flag through every call
//! site. Serve/mount filesystem backends construct a `NetworkApi` per
//! operation deep inside long-lived trait impls, far from where the flag is
//! parsed — reading the global toggle here (mirrors `output::is_json()`) is
//! the same reason `IXR_USER`/`IXR_WORKSPACE_ID` are read directly rather
//! than threaded through call chains.

use internxt_core::network::{NetworkApi, NetworkTimeouts};

/// Build a `NetworkApi`, honoring `--no-timeout`/`IXR_NO_TIMEOUTS` by
/// disabling its idle-read timeout. `connect_timeout` stays on regardless —
/// a hung connection attempt is unrelated to transfer speed and should still
/// fail fast.
pub fn network_api(bridge_user: &str, user_id: &str) -> NetworkApi {
    if crate::output::no_timeout() {
        let timeouts = NetworkTimeouts {
            read: None,
            ..NetworkTimeouts::default()
        };
        NetworkApi::with_timeouts(bridge_user, user_id, timeouts)
    } else {
        NetworkApi::new(bridge_user, user_id)
    }
}

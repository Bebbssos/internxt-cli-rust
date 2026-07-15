//! Static configuration. Mirrors og/cli .env.template defaults.
//! Values can be overridden via environment variables of the same name.
//!
//! Only API endpoints + app constants live here — no filesystem paths. Where the
//! front-end stores credentials is the front-end's concern (it owns persistence).

pub fn get(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

pub fn drive_web_url() -> String {
    get("DRIVE_WEB_URL", "https://drive.internxt.com")
}

/// Drive / auth REST API base (DRIVE_NEW_API_URL in the node CLI).
pub fn drive_api_url() -> String {
    get("DRIVE_NEW_API_URL", "https://gateway.internxt.com/drive")
}

/// Network (bridge) base url.
pub fn network_url() -> String {
    get("NETWORK_URL", "https://gateway.internxt.com/network")
}

/// Secret used for CryptoJS-compatible AES of salt / password hash / credentials file.
pub fn app_crypto_secret() -> String {
    get("APP_CRYPTO_SECRET", "6KYQBP847D4ATSFA")
}

pub fn desktop_header() -> String {
    get("DESKTOP_HEADER", "3b68706a367fd567b929396290b1de40768bb768")
}

pub const CLIENT_NAME: &str = "internxt-cli";
pub const CLIENT_VERSION: &str = "1.6.5";

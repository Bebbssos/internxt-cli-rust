//! Per-file upload size validation. Mirrors og's `UploadUtils.checkUploadSizeLimits`
//! (the plan's `maxUploadFileSize` from `GET /files/limits`) but with **no**
//! hard-coded default cap: when the plan sets no limit, uploads are unbounded.
//!
//! Every upload path (`upload-file`, `upload-folder`, `sync-up`, and the `serve`
//! / `mount` backends) resolves one [`UploadLimit`] up front and calls
//! [`UploadLimit::check`] per file. Resolution precedence:
//!
//! 1. `--no-upload-limit` flag        → unlimited
//! 2. `--max-upload-size <SIZE>` flag → that cap
//! 3. `INTERNXT_MAX_UPLOAD_SIZE` env  → `off`/`none`/… = unlimited, else a cap
//! 4. otherwise                       → the plan's cap (or unlimited)

use anyhow::{anyhow, Result};
use internxt_core::api::DriveApi;
use internxt_core::models::Credentials;

/// Universal env var: applies to every upload command. A size string
/// (`5GB`, `500M`, raw bytes) sets a custom cap; a disable word
/// (`off`/`none`/`no`/`false`/`unlimited`/`disabled`/`0`) removes the limit.
pub const ENV_VAR: &str = "INTERNXT_MAX_UPLOAD_SIZE";

/// The upload-limit CLI flags, shared (via `#[command(flatten)]`) by every
/// upload command.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct UploadLimitArgs {
    /// Disable the per-file upload size limit entirely (overrides everything else).
    #[arg(long, default_value_t = false)]
    pub no_upload_limit: bool,
    /// Custom per-file upload size cap, e.g. `5GB`, `500M`, or raw bytes.
    /// Overrides the plan's cap. Also settable via the INTERNXT_MAX_UPLOAD_SIZE
    /// env var (which additionally accepts `off`/`none`/… to disable).
    #[arg(long, value_name = "SIZE")]
    pub max_upload_size: Option<String>,
}

/// A resolved effective cap. `None` = unlimited.
#[derive(Clone, Copy, Debug)]
pub struct UploadLimit(Option<u64>);

impl UploadLimit {
    /// Reject when `size` exceeds the cap. Message mirrors og's wording.
    pub fn check(&self, size: u64) -> Result<()> {
        if let Some(max) = self.0 {
            if size > max {
                return Err(anyhow!(
                    "File is too big ({} exceeds upload limit of {})",
                    human_size(size),
                    human_size(max)
                ));
            }
        }
        Ok(())
    }
}

/// Resolve the effective upload cap from flags, env, then the plan endpoint.
/// The plan lookup hits `GET /files/limits` only when neither a flag nor the
/// env var decides the outcome.
pub async fn resolve(args: &UploadLimitArgs, creds: &Credentials) -> Result<UploadLimit> {
    if args.no_upload_limit {
        return Ok(UploadLimit(None));
    }
    if let Some(s) = &args.max_upload_size {
        return Ok(UploadLimit(Some(parse_size(s)?)));
    }
    if let Ok(v) = std::env::var(ENV_VAR) {
        let v = v.trim();
        if !v.is_empty() {
            if is_disable_word(v) {
                return Ok(UploadLimit(None));
            }
            return Ok(UploadLimit(Some(parse_size(v).map_err(|e| {
                anyhow!("invalid {ENV_VAR}: {e}")
            })?)));
        }
    }
    // No override: use the plan's cap (no local default — unlimited if unset).
    let api = DriveApi::for_credentials(creds);
    let limit = api.get_file_limits(&creds.token).await?;
    Ok(UploadLimit(limit))
}

fn is_disable_word(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "off" | "none" | "no" | "false" | "unlimited" | "disable" | "disabled" | "0"
    )
}

/// Parse a size like `1048576`, `100`, `5K`, `5KB`, `500M`, `10GB`, `2T` into
/// bytes. Units are binary (1024-based); a bare number is bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty size"));
    }
    // Split trailing non-digit unit off the leading number.
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ' ' && c != '_')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .trim()
        .replace('_', "")
        .parse()
        .map_err(|_| anyhow!("not a number: {s:?}"))?;
    if num < 0.0 {
        return Err(anyhow!("negative size: {s:?}"));
    }
    let mult: f64 = match unit.trim().trim_end_matches(['b', 'B']).to_ascii_lowercase().as_str() {
        "" => 1.0,
        "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "p" => 1024.0_f64.powi(5),
        other => return Err(anyhow!("unknown size unit: {other:?}")),
    };
    Ok((num * mult).round() as u64)
}

/// Human-readable binary size for error messages (matches og's `humanFileSize`).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut val = bytes as f64;
    let mut i = 0;
    while val >= 1024.0 && i < UNITS.len() - 1 {
        val /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{val:.2} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("5K").unwrap(), 5 * 1024);
        assert_eq!(parse_size("2MB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1.5G").unwrap(), (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("5XB").is_err());
    }

    #[test]
    fn disable_words() {
        for w in ["off", "None", "NO", "unlimited", "0", "disabled"] {
            assert!(is_disable_word(w));
        }
        assert!(!is_disable_word("5GB"));
    }

    #[test]
    fn check_rejects_over() {
        let l = UploadLimit(Some(100));
        assert!(l.check(100).is_ok());
        assert!(l.check(101).is_err());
        assert!(UploadLimit(None).check(u64::MAX).is_ok());
    }
}

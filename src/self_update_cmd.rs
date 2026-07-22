//! `ixr update` — replaces the running binary with the latest GitHub release.
//! Only meaningful for the standalone binary distribution (see `Commands::Update`
//! doc comment in main.rs for why package-manager installs should skip this).

use crate::output;
use anyhow::{Context, Result, anyhow};
use serde_json::json;

fn owner_and_repo() -> Result<(&'static str, &'static str)> {
    // e.g. "https://github.com/Bebbssos/internxt-cli-rust" -> ("Bebbssos", "internxt-cli-rust")
    let repo_url = env!("CARGO_PKG_REPOSITORY");
    let path = repo_url
        .trim_end_matches('/')
        .rsplit("github.com/")
        .next()
        .context("CARGO_PKG_REPOSITORY is not a github.com URL")?;
    let mut parts = path.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty());
    let repo = parts.next().filter(|s| !s.is_empty());
    owner.zip(repo).context("could not parse owner/repo from CARGO_PKG_REPOSITORY")
}

pub async fn run(check: bool, yes: bool, pre_release: bool) -> Result<()> {
    tokio::task::spawn_blocking(move || run_blocking(check, yes, pre_release))
        .await
        .context("update task panicked")?
}

fn run_blocking(check: bool, yes: bool, pre_release: bool) -> Result<()> {
    let (owner, repo) = owner_and_repo()?;
    let current = env!("CARGO_PKG_VERSION");

    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(owner)
        .repo_name(repo)
        .build()?
        .fetch()?;
    // GitHub lists releases newest-created-first, prereleases included; skip
    // any release whose tag parses as a semver prerelease (e.g. v0.2.0-rc.1)
    // unless the caller opted in with --pre-release.
    let latest = releases
        .iter()
        .find(|r| {
            pre_release
                || semver::Version::parse(r.version.trim_start_matches('v'))
                    .map(|v| v.pre.is_empty())
                    .unwrap_or(true)
        })
        .ok_or_else(|| anyhow!("no releases found for {owner}/{repo}"))?;
    let latest_version = latest.version.trim_start_matches('v').to_string();

    // Semver, not string equality: a prerelease (0.1.0-rc.1) must never
    // outrank the 0.1.0 it precedes, and this must agree with what
    // `Update::update()` below decides on its own.
    let current_semver = semver::Version::parse(current).context("CARGO_PKG_VERSION is not valid semver")?;
    let latest_semver = semver::Version::parse(&latest_version)
        .with_context(|| format!("release tag v{latest_version} is not valid semver"))?;

    if latest_semver <= current_semver {
        output::emit(
            &format!("✓ Already up to date (v{current})."),
            json!({ "current": current, "latest": latest_version, "updated": false }),
        );
        return Ok(());
    }

    if check {
        output::emit(
            &format!("A newer release is available: v{current} -> v{latest_version}. Run `ixr update` to install."),
            json!({ "current": current, "latest": latest_version, "updated": false }),
        );
        return Ok(());
    }

    if !yes {
        if output::is_json() || output::is_non_interactive() {
            return Err(anyhow!(
                "The \"--yes\" flag is required to install updates in JSON / non-interactive mode."
            ));
        }
        use std::io::Write;
        print!("Update ixr v{current} -> v{latest_version}? (y/N) ");
        std::io::stdout().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        if s.trim().to_lowercase().chars().next() != Some('y') {
            return Err(anyhow!("User confirmation is required to install the update."));
        }
    }

    let target = self_update::get_target();
    let status = self_update::backends::github::Update::configure()
        .repo_owner(owner)
        .repo_name(repo)
        .bin_name("ixr")
        .target(target)
        .show_download_progress(!output::is_json())
        .current_version(current)
        .no_confirm(true)
        .build()?
        .update()?;

    output::emit(
        &format!("✓ Updated to v{}.", status.version()),
        json!({ "current": current, "latest": status.version(), "updated": true }),
    );
    Ok(())
}

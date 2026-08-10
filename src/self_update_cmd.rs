//! `ixr update` — replaces the running binary with the latest GitHub release.
//! Only meaningful for the standalone binary distribution (see `Commands::Update`
//! doc comment in main.rs for why package-manager installs should skip this).

use crate::output;
use anyhow::{Context, Result, anyhow};
use self_update::update::Release;
use serde_json::json;
use std::cmp::Ordering;

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

/// Releases sorted newest-first by semver (GitHub's own "newest created
/// first" order is a close approximation but not guaranteed to match semver
/// order, e.g. after a backported patch release).
fn sorted_releases(releases: &[Release]) -> Vec<(semver::Version, &Release)> {
    let mut parsed: Vec<(semver::Version, &Release)> = releases
        .iter()
        .filter_map(|r| semver::Version::parse(r.version.trim_start_matches('v')).ok().map(|v| (v, r)))
        .collect();
    parsed.sort_by(|a, b| b.0.cmp(&a.0));
    parsed
}

pub async fn run(check: bool, yes: bool, pre_release: bool, patch_only: bool, version: Option<String>) -> Result<()> {
    tokio::task::spawn_blocking(move || run_blocking(check, yes, pre_release, patch_only, version))
        .await
        .context("update task panicked")?
}

fn run_blocking(check: bool, yes: bool, pre_release: bool, patch_only: bool, version: Option<String>) -> Result<()> {
    let (owner, repo) = owner_and_repo()?;
    let current = env!("CARGO_PKG_VERSION");
    let current_semver = semver::Version::parse(current).context("CARGO_PKG_VERSION is not valid semver")?;

    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(owner)
        .repo_name(repo)
        .build()?
        .fetch()?;
    let all_sorted = sorted_releases(&releases);

    let (target_semver, is_explicit) = if let Some(requested) = version {
        let requested = requested.trim_start_matches('v');
        let requested_semver =
            semver::Version::parse(requested).with_context(|| format!("`--version {requested}` is not valid semver"))?;
        if !all_sorted.iter().any(|(v, _)| v == &requested_semver) {
            return Err(anyhow!("no release found for v{requested} in {owner}/{repo}"));
        }
        (requested_semver, true)
    } else {
        // Newest release first, skipping prereleases unless opted in.
        let mut candidates = all_sorted.iter().filter(|(v, _)| pre_release || v.pre.is_empty());

        let picked = if patch_only {
            // Same major.minor, higher patch only — mirrors semver-caret
            // "compatible" for 0.x (only patch bumps are non-breaking).
            candidates.find(|(v, _)| {
                v.major == current_semver.major && v.minor == current_semver.minor && v.patch > current_semver.patch
            })
        } else {
            candidates.next()
        };

        let Some((picked_semver, _)) = picked else {
            output::emit(
                &format!("✓ Already up to date (v{current})."),
                json!({ "current": current, "target": current, "updated": false }),
            );
            return Ok(());
        };
        (picked_semver.clone(), false)
    };

    let cmp = target_semver.cmp(&current_semver);
    if !is_explicit && cmp != Ordering::Greater {
        output::emit(
            &format!("✓ Already up to date (v{current})."),
            json!({ "current": current, "target": current, "updated": false }),
        );
        return Ok(());
    }
    if is_explicit && cmp == Ordering::Equal {
        output::emit(
            &format!("✓ Already at v{current}."),
            json!({ "current": current, "target": target_semver.to_string(), "updated": false }),
        );
        return Ok(());
    }

    let downgrading = cmp == Ordering::Less;
    let verb = if downgrading { "Downgrade" } else { "Update" };

    // When the caller didn't ask for --patch-only or an explicit --version,
    // flag it if a filtered-out newer release exists, so the messaging never
    // silently disagrees with what a plain `ixr update` would target.
    let mut note = String::new();
    if !is_explicit && patch_only {
        let true_latest = all_sorted.iter().find(|(v, _)| pre_release || v.pre.is_empty());
        if let Some((true_latest, _)) = true_latest
            && *true_latest > target_semver
        {
            note = format!(" (v{true_latest} is also available; run without --patch-only to get it)");
        }
    }

    if check {
        // Reproduce the exact same version-selection flags, so the printed
        // command actually installs what was just reported instead of
        // falling back to plain `ixr update`'s (possibly different) pick.
        let mut repro = String::new();
        if is_explicit {
            repro.push_str(&format!(" --version {target_semver}"));
        } else {
            if pre_release {
                repro.push_str(" --pre-release");
            }
            if patch_only {
                repro.push_str(" --patch-only");
            }
        }
        output::emit(
            &format!("{verb} available: v{current} -> v{target_semver}.{note} Run `ixr update{repro}` to install."),
            json!({ "current": current, "target": target_semver.to_string(), "downgrade": downgrading, "updated": false }),
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
        print!("{verb} ixr v{current} -> v{target_semver}?{note} (y/N) ");
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
        // Pin the exact tag: without this, self_update re-derives "latest"
        // using its own semver-caret compatibility filter, which can disagree
        // with the version just shown/confirmed above (see git history for
        // the bug this caused — a `0.2.0 -> 0.3.0` prompt that only installed
        // `0.2.1`).
        .target_version_tag(&format!("v{target_semver}"))
        .show_download_progress(!output::is_json())
        .current_version(current)
        .no_confirm(true)
        .build()?
        .update()?;

    output::emit(
        &format!("✓ Updated to v{}.", status.version()),
        json!({ "current": current, "target": status.version(), "downgrade": downgrading, "updated": true }),
    );
    Ok(())
}

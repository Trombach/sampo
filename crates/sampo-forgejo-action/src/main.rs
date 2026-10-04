mod error;
mod forgejo;
mod git;
mod sampo;

use crate::error::{ActionError, Result};
use crate::forgejo::ForgejoClient;
use crate::sampo::ReleasePlan;
use glob::glob;
use sampo_core::errors::SampoError;
use sampo_core::workspace::discover_workspace;
use sampo_core::{Config as SampoConfig, PublishExtraArgs, current_branch};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// Error type and Result are provided by sampo-core::errors

#[derive(Debug, Clone)]
struct ForgejoReleaseOptions {
    /// Create Forgejo Release for newly created tags during publish
    create_forgejo_release: bool,
    /// Release asset patterns provided by the workflow (already-built artifacts)
    asset_specs: Vec<AssetSpec>,
}

impl ForgejoReleaseOptions {
    fn from_config(config: &Config) -> Self {
        Self {
            create_forgejo_release: config.create_forgejo_release,
            asset_specs: parse_asset_specs(config.release_assets.as_deref()),
        }
    }
}

#[derive(Debug, Clone)]
struct AssetSpec {
    pattern: String,
    rename: Option<String>,
}

#[derive(Debug)]
struct ResolvedAsset {
    path: PathBuf,
    asset_name: String,
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Auto,
    Release,
    Publish,
}

impl Mode {
    fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "automatic" => Mode::Auto,
            "release" => Mode::Release,
            "publish" => Mode::Publish,
            _ => Mode::Auto,
        }
    }
}

/// Sampo Forgejo Action configuration
///
/// This configuration reads inputs from Forgejo Actions environment variables.
/// Forgejo Actions inputs are exposed as INPUT_* environment variables.
#[derive(Debug)]
struct Config {
    /// Which operation to run (release, publish, or both)
    mode: Mode,

    /// Simulate actions without changing files or publishing artifacts
    dry_run: bool,

    /// Path to the repository root (defaults to FORGEJO_WORKSPACE)
    working_directory: Option<PathBuf>,

    /// Optional crates.io token (exported to child processes)
    cargo_token: Option<String>,

    /// Extra args passed through to `sampo publish` (after `--`)
    /// Accepts a single string that will be split on whitespace.
    args: Option<String>,

    /// Extra arguments forwarded only to `cargo publish`
    cargo_args: Option<String>,

    /// Extra arguments forwarded only to npm/pnpm/yarn/bun publish
    npm_args: Option<String>,

    /// Extra arguments forwarded only to `mix hex.publish`
    hex_args: Option<String>,

    /// Extra arguments forwarded only to PyPI/twine upload
    pypi_args: Option<String>,

    /// Extra arguments forwarded only to Packagist/Composer
    packagist_args: Option<String>,

    /// Extra arguments forwarded only to Maven
    maven_args: Option<String>,

    /// Base branch for the Release PR (default: current ref name or 'main')
    base_branch: Option<String>,

    /// Branch name to use for the Release PR (default: 'release/sampo')
    pr_branch: Option<String>,

    /// Title to use for the Release PR (default: 'Release')
    pr_title: Option<String>,

    /// Branch name to use for the Stabilize PR (default: 'stabilize/<branch>')
    stabilize_pr_branch: Option<String>,

    /// Title to use for the Stabilize PR (default: 'Release stable (<branch>)')
    stabilize_pr_title: Option<String>,

    /// Create Forgejo releases for newly created tags during publish
    create_forgejo_release: bool,

    /// Paths or glob patterns to upload as release assets (comma or newline separated)
    release_assets: Option<String>,
}

impl Config {
    /// Load configuration from GitHub Actions environment variables
    fn from_environment() -> Self {
        let mode = std::env::var("INPUT_COMMAND")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| Mode::parse(&v))
            .unwrap_or(Mode::Auto);

        let dry_run = std::env::var("INPUT_DRY_RUN")
            .map(|v| v.eq_ignore_ascii_case("true") || v.trim() == "1")
            .unwrap_or(false);

        let working_directory = std::env::var("INPUT_WORKING_DIRECTORY")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);

        let cargo_token = std::env::var("INPUT_CARGO_TOKEN")
            .ok()
            .filter(|v| !v.is_empty());

        let args = std::env::var("INPUT_ARGS").ok().filter(|v| !v.is_empty());

        let cargo_args = std::env::var("INPUT_CARGO_ARGS")
            .ok()
            .filter(|v| !v.is_empty());

        let npm_args = std::env::var("INPUT_NPM_ARGS")
            .ok()
            .filter(|v| !v.is_empty());

        let hex_args = std::env::var("INPUT_HEX_ARGS")
            .ok()
            .filter(|v| !v.is_empty());

        let pypi_args = std::env::var("INPUT_PYPI_ARGS")
            .ok()
            .filter(|v| !v.is_empty());

        let packagist_args = std::env::var("INPUT_PACKAGIST_ARGS")
            .ok()
            .filter(|v| !v.is_empty());

        let maven_args = std::env::var("INPUT_MAVEN_ARGS")
            .ok()
            .filter(|v| !v.is_empty());

        let base_branch = std::env::var("INPUT_BASE_BRANCH")
            .ok()
            .filter(|v| !v.is_empty());

        let pr_branch = std::env::var("INPUT_PR_BRANCH")
            .ok()
            .filter(|v| !v.is_empty());

        let pr_title = std::env::var("INPUT_PR_TITLE")
            .ok()
            .filter(|v| !v.is_empty());

        let stabilize_pr_branch = std::env::var("INPUT_STABILIZE_PR_BRANCH")
            .ok()
            .filter(|v| !v.is_empty());

        let stabilize_pr_title = std::env::var("INPUT_STABILIZE_PR_TITLE")
            .ok()
            .filter(|v| !v.is_empty());

        let create_forgejo_release = std::env::var("INPUT_CREATE_FORGEJO_RELEASE")
            .map(|v| v.eq_ignore_ascii_case("true") || v.trim() == "1")
            .unwrap_or(false);

        let release_assets = std::env::var("INPUT_RELEASE_ASSETS")
            .ok()
            .filter(|v| !v.is_empty());

        Self {
            mode,
            dry_run,
            working_directory,
            cargo_token,
            args,
            cargo_args,
            npm_args,
            hex_args,
            pypi_args,
            packagist_args,
            maven_args,
            base_branch,
            pr_branch,
            pr_title,
            stabilize_pr_branch,
            stabilize_pr_title,
            create_forgejo_release,
            release_assets,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let config = Config::from_environment();

    let workspace = determine_workspace(&config)?;

    let repo_config = SampoConfig::load(&workspace).unwrap_or_default();

    let branch = current_branch()?;
    if !repo_config.is_release_branch(&branch) {
        return Err(SampoError::Release(format!(
            "Branch '{}' is not listed in git.release_branches (allowed: {:?})",
            branch,
            repo_config
                .release_branches()
                .into_iter()
                .collect::<Vec<_>>()
        ))
        .into());
    }

    unsafe {
        std::env::set_var("SAMPO_RELEASE_BRANCH", &branch);
    }

    // Execute the requested operations
    let (released, published) = execute_operations(&config, &workspace, &repo_config, &branch)?;

    // Emit outputs for the workflow
    emit_forgejo_output("released", released)?;
    emit_forgejo_output("published", published)?;

    Ok(())
}

/// Determine the workspace root directory
fn determine_workspace(config: &Config) -> Result<PathBuf> {
    config
        .working_directory
        .clone()
        .or_else(|| std::env::var("GITHUB_WORKSPACE").ok().map(PathBuf::from))
        .ok_or(ActionError::NoWorkingDirectory)
}

/// Execute the requested operations and return (released, published) status
///
/// In `auto` mode we always run a dry `sampo release` first (`capture_release_plan`).
/// If there are pending changesets we prepare/update the release PR (which runs a
/// real `sampo release`). Otherwise we fall back to `post_merge_publish`, which
/// reuses the same publish pipeline as the explicit `publish` mode. That pipeline
/// only publishes crates that still need it (via `sampo_core::run_publish`) and
/// reports `published = true` exclusively when fresh git tags were produced, so a
/// plain commit without changesets will exit cleanly without pushing anything.
fn execute_operations(
    config: &Config,
    workspace: &Path,
    repo_config: &SampoConfig,
    branch: &str,
) -> Result<(bool, bool)> {
    let mut released = false;
    let mut published = false;

    match config.mode {
        Mode::Auto => {
            let plan = sampo::capture_release_plan(workspace)?;
            if plan.has_changes {
                println!(
                    "Detected {} pending release package(s); preparing release PR.",
                    plan.releases.len()
                );
                let plan_requires_stabilize = plan_includes_prerelease(&plan.releases);
                let forgejo_client = create_forgejo_client()?;
                let release_prepared = prepare_release_pr(
                    workspace,
                    config,
                    repo_config,
                    branch,
                    &forgejo_client,
                    Some(plan),
                )?;
                let stabilize_prepared = if release_prepared && plan_requires_stabilize {
                    prepare_stabilize_pr(workspace, config, repo_config, branch, &forgejo_client)?
                } else {
                    if plan_requires_stabilize && !release_prepared {
                        println!(
                            "Skipped stabilize PR because release PR preparation did not complete."
                        );
                    }
                    false
                };
                released = release_prepared || stabilize_prepared;
            } else {
                // Prerelease packages with preserved changesets need a stabilize PR even
                // when there are no pending changesets in the regular changesets directory.
                let prerelease_packages = collect_prerelease_packages(workspace)?;
                if !prerelease_packages.is_empty() {
                    println!(
                        "No pending changesets but prerelease packages exist on branch '{}'; checking for stabilize PR.",
                        branch
                    );
                    let github_client = create_forgejo_client()?;
                    released = prepare_stabilize_pr(
                        workspace,
                        config,
                        repo_config,
                        branch,
                        &github_client,
                    )?;
                }
                println!(
                    "No pending changesets found on branch '{}'. Checking for merged releases to publish.",
                    branch
                );
                let forgejo_options = ForgejoReleaseOptions::from_config(config);
                let forgejo_client = if forgejo_options.create_forgejo_release {
                    Some(create_forgejo_client()?)
                } else {
                    None
                };
                let extra_args = build_publish_extra_args(config);
                published = post_merge_publish(
                    workspace,
                    config.dry_run,
                    &extra_args,
                    config.cargo_token.as_deref(),
                    &forgejo_options,
                    forgejo_client.as_ref(),
                )?;
            }
        }
        Mode::Release => {
            sampo::run_release(workspace, config.dry_run, config.cargo_token.as_deref())?;
            released = true;
        }
        Mode::Publish => {
            let forgejo_options = ForgejoReleaseOptions::from_config(config);
            let forgejo_client = if forgejo_options.create_forgejo_release {
                Some(create_forgejo_client()?)
            } else {
                None
            };
            let extra_args = build_publish_extra_args(config);
            published = post_merge_publish(
                workspace,
                config.dry_run,
                &extra_args,
                config.cargo_token.as_deref(),
                &forgejo_options,
                forgejo_client.as_ref(),
            )?;
        }
    }

    Ok((released, published))
}

/// Emit a Forgejo Actions output
fn emit_forgejo_output(key: &str, value: bool) -> Result<()> {
    let value_str = if value { "true" } else { "false" };

    if let Some(path) = std::env::var_os("FORGEJO_OUTPUT") {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}={}", key, value_str)?;
    }

    Ok(())
}

/// Create a Forgejo client if credentials are available
fn create_forgejo_client() -> Result<forgejo::ForgejoClient> {
    let base_url =
        std::env::var("BASE_URL").map_err(|_| ActionError::ForgejoCredentialsNotAvailable)?;
    let repo = std::env::var("FORGEJO_REPOSITORY")
        .map_err(|_| ActionError::ForgejoCredentialsNotAvailable)?;
    let token =
        std::env::var("FORGEJO_TOKEN").map_err(|_| ActionError::ForgejoCredentialsNotAvailable)?;

    if repo.is_empty() || token.is_empty() {
        return Err(ActionError::ForgejoCredentialsNotAvailable);
    }

    ForgejoClient::new(base_url, repo, token)
}

fn prepare_release_pr(
    workspace: &Path,
    config: &Config,
    repo_config: &SampoConfig,
    branch: &str,
    forgejo_client: &forgejo::ForgejoClient,
    provided_plan: Option<ReleasePlan>,
) -> Result<bool> {
    let plan = match provided_plan {
        Some(plan) => plan,
        None => sampo::capture_release_plan(workspace)?,
    };

    if !plan.has_changes {
        println!("No changesets detected. Skipping PR preparation.");
        return Ok(false);
    }

    let releases = &plan.releases;

    // Configuration
    let base_branch = config
        .base_branch
        .clone()
        .unwrap_or_else(|| branch.to_string());
    let is_prerelease = plan_includes_prerelease(releases);
    let pr_branch = config
        .pr_branch
        .clone()
        .unwrap_or_else(|| default_pr_branch(branch, is_prerelease));
    let pr_title = config
        .pr_title
        .clone()
        .unwrap_or_else(|| default_pr_title(branch, is_prerelease));

    // Build PR body BEFORE running release (release will consume changesets)
    let pr_body = {
        // Load configuration for dependency explanations
        let body = sampo::build_release_pr_body(workspace, releases, repo_config)?;
        if body.trim().is_empty() {
            println!("No applicable package changes for PR body. Skipping PR creation.");
            return Ok(false);
        }
        body
    };

    // Setup git
    git::setup_bot_user(workspace)?;
    git::git(&["fetch", "origin", "--prune"], Some(workspace))?;

    // Create release branch
    git::git(
        &[
            "checkout",
            "-B",
            &pr_branch,
            &format!("origin/{}", base_branch),
        ],
        Some(workspace),
    )?;

    // Apply release (no tags)
    sampo::run_release(workspace, false, config.cargo_token.as_deref())?;

    // Check for changes and commit
    if !git::has_changes(workspace)? {
        println!("No file changes after release. Skipping commit/PR.");
        git::git(&["checkout", branch], Some(workspace))?;
        return Ok(false);
    }

    git::git(&["add", "-A"], Some(workspace))?;
    git::git(
        &[
            "commit",
            "-m",
            "chore(release): bump versions and changelogs",
        ],
        Some(workspace),
    )?;

    // Force push to release branch (overwrites any existing branch)
    git::git(
        &["push", "origin", &format!("HEAD:{}", pr_branch), "--force"],
        Some(workspace),
    )?;

    // Create PR
    forgejo_client.ensure_pull_request(&pr_branch, &base_branch, &pr_title, &pr_body)?;

    // Switch back to the release branch's base to keep the workspace ready for subsequent steps
    git::git(&["checkout", branch], Some(workspace))?;

    println!(
        "Prepared release PR with {} pending package(s).",
        releases.len()
    );

    Ok(true)
}

fn prepare_stabilize_pr(
    workspace: &Path,
    config: &Config,
    repo_config: &SampoConfig,
    branch: &str,
    forgejo_client: &forgejo::ForgejoClient,
) -> Result<bool> {
    let prerelease_packages = collect_prerelease_packages(workspace)?;
    if prerelease_packages.is_empty() {
        println!("Workspace packages are already stable. Skipping stabilize PR.");
        return Ok(false);
    }

    let base_branch = config
        .base_branch
        .clone()
        .unwrap_or_else(|| branch.to_string());
    let pr_branch = config
        .stabilize_pr_branch
        .clone()
        .unwrap_or_else(|| default_stabilize_pr_branch(branch));
    let pr_title = config
        .stabilize_pr_title
        .clone()
        .unwrap_or_else(|| default_stabilize_pr_title(branch));

    git::setup_bot_user(workspace)?;
    git::git(&["fetch", "origin", "--prune"], Some(workspace))?;
    git::git(
        &[
            "checkout",
            "-B",
            &pr_branch,
            &format!("origin/{}", base_branch),
        ],
        Some(workspace),
    )?;

    let plan = sampo::capture_stabilize_plan(workspace)?;
    if !plan.has_changes {
        println!("No stable release changes detected. Skipping stabilize PR.");
        git::git(
            &["reset", "--hard", &format!("origin/{}", base_branch)],
            Some(workspace),
        )?;
        git::git(&["checkout", branch], Some(workspace))?;
        return Ok(false);
    }

    let pr_body = {
        let body = sampo::build_release_pr_body(workspace, &plan.releases, repo_config)?;
        if body.trim().is_empty() {
            println!("No applicable package changes for stabilize PR body. Skipping PR creation.",);
            git::git(
                &["reset", "--hard", &format!("origin/{}", base_branch)],
                Some(workspace),
            )?;
            git::git(&["checkout", branch], Some(workspace))?;
            return Ok(false);
        }
        body
    };

    sampo::run_stabilize_release(workspace, false, config.cargo_token.as_deref())?;

    if !git::has_changes(workspace)? {
        println!("No file changes after stabilize release. Skipping commit/PR.");
        git::git(
            &["reset", "--hard", &format!("origin/{}", base_branch)],
            Some(workspace),
        )?;
        git::git(&["checkout", branch], Some(workspace))?;
        return Ok(false);
    }

    git::git(&["add", "-A"], Some(workspace))?;
    git::git(
        &[
            "commit",
            "-m",
            "chore(release): stabilize versions and changelogs",
        ],
        Some(workspace),
    )?;

    git::git(
        &["push", "origin", &format!("HEAD:{}", pr_branch), "--force"],
        Some(workspace),
    )?;

    forgejo_client.ensure_pull_request(&pr_branch, &base_branch, &pr_title, &pr_body)?;
    git::git(&["checkout", branch], Some(workspace))?;

    println!(
        "Prepared stabilize PR with {} pending package(s).",
        plan.releases.len()
    );

    Ok(true)
}

fn sanitized_branch_name(branch: &str) -> String {
    branch.replace('/', "-")
}

fn plan_includes_prerelease(releases: &BTreeMap<String, (String, String, String)>) -> bool {
    releases.values().any(|(_, _, new_version)| {
        Version::parse(new_version)
            .map(|version| !version.pre.is_empty())
            .unwrap_or(false)
    })
}

fn collect_prerelease_packages(workspace: &Path) -> Result<Vec<String>> {
    let ws = discover_workspace(workspace).map_err(|e| ActionError::SampoCommandFailed {
        operation: "workspace-discovery".to_string(),
        message: e.to_string(),
    })?;

    let mut names: Vec<String> = ws
        .members
        .iter()
        .filter(|info| info.version.contains('-'))
        .map(|info| info.identifier.clone())
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn default_pr_branch(branch: &str, is_prerelease: bool) -> String {
    let suffix = sanitized_branch_name(branch);
    if is_prerelease {
        format!("pre-release/{}", suffix)
    } else {
        format!("release/{}", suffix)
    }
}

fn default_pr_title(branch: &str, is_prerelease: bool) -> String {
    if is_prerelease {
        format!("Pre-release ({})", branch)
    } else {
        format!("Release ({})", branch)
    }
}

fn default_stabilize_pr_branch(branch: &str) -> String {
    format!("stabilize/{}", sanitized_branch_name(branch))
}

fn default_stabilize_pr_title(branch: &str) -> String {
    format!("Release stable ({})", branch)
}

fn parse_args_string(s: Option<&str>) -> Vec<String> {
    s.map(|a| a.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

fn build_publish_extra_args(config: &Config) -> PublishExtraArgs {
    PublishExtraArgs {
        universal: parse_args_string(config.args.as_deref()),
        cargo: parse_args_string(config.cargo_args.as_deref()),
        npm: parse_args_string(config.npm_args.as_deref()),
        hex: parse_args_string(config.hex_args.as_deref()),
        pypi: parse_args_string(config.pypi_args.as_deref()),
        packagist: parse_args_string(config.packagist_args.as_deref()),
        maven: parse_args_string(config.maven_args.as_deref()),
    }
}

/// Run `sampo publish` and handle the post-merge duties (tag push, GitHub releases).
/// Returns true only when new tags were created/pushed, so the workflow can tell if a
/// real publish happened. Combined with `sampo_core::run_publish` (which skips crates
/// already published or marked `publish = false`), this prevents accidental publishes
/// on commits sans changesets: the action simply logs "No new tags" and exits.
fn post_merge_publish(
    workspace: &Path,
    dry_run: bool,
    extra_args: &PublishExtraArgs,
    cargo_token: Option<&str>,
    forgejo_options: &ForgejoReleaseOptions,
    forgejo_client: Option<&forgejo::ForgejoClient>,
) -> Result<bool> {
    // Setup git identity for tag creation
    git::setup_bot_user(workspace)?;

    // Publish and get information about tags created/would-be-created
    let publish_output = sampo::run_publish(workspace, dry_run, extra_args, cargo_token)?;
    let new_tags = publish_output.tags;

    if !dry_run && !new_tags.is_empty() {
        println!("Pushing {} new tags", new_tags.len());
        for tag in &new_tags {
            git::git(&["push", "origin", tag], Some(workspace))?;
        }
    } else if dry_run && !new_tags.is_empty() {
        println!(
            "Would push {} new tags (skipped in dry-run mode)",
            new_tags.len()
        );
        for tag in &new_tags {
            println!("  - {}", tag);
        }
    }

    if !dry_run
        && forgejo_options.create_forgejo_release
        && !new_tags.is_empty()
        && let Some(client) = forgejo_client
    {
        for tag in &new_tags {
            println!("Creating GitHub release for {}", tag);
            create_forgejo_release_for_tag(client, tag, workspace, forgejo_options)?;
        }
    } else if dry_run && forgejo_options.create_forgejo_release && !new_tags.is_empty() {
        println!(
            "Would create {} Forgejo releases (skipped in dry-run mode)",
            new_tags.len()
        );
        for tag in &new_tags {
            println!("  - {}", tag);
        }
    }

    let published = !dry_run && !new_tags.is_empty();
    if !published && !dry_run {
        println!("No new tags were created during publish.");
    }

    Ok(published)
}

fn create_forgejo_release_for_tag(
    forgejo_client: &forgejo::ForgejoClient,
    tag: &str,
    workspace: &Path,
    forgejo_options: &ForgejoReleaseOptions,
) -> Result<()> {
    let config = SampoConfig::load(workspace).ok();

    let body = match build_release_body_from_changelog(workspace, tag) {
        Some(body) => body,
        None => format!("Automated release for tag {}", tag),
    };

    let upload_url = match forgejo_client.create_release(
        tag,
        &body,
        tag_is_prerelease_with_config(tag, config.as_ref()),
    ) {
        Ok(upload_url) => upload_url,
        Err(e) => {
            eprintln!(
                "Warning: Failed to create Forgejo release for {}: {}",
                tag, e
            );
            return Ok(());
        }
    };

    if !upload_url.is_empty() {
        let assets = resolve_release_assets(workspace, tag, &forgejo_options.asset_specs)?;
        if assets.is_empty() {
            if !forgejo_options.asset_specs.is_empty() {
                eprintln!(
                    "Warning: No release assets matched the configured patterns for {}.",
                    tag
                );
            }
        } else {
            for asset in assets {
                match forgejo_client.upload_release_asset(
                    &upload_url,
                    &asset.path,
                    &asset.asset_name,
                ) {
                    Ok(()) => {
                        println!(
                            "Uploaded release asset '{}' from {}",
                            asset.asset_name,
                            asset.path.display()
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "Warning: Failed to upload release asset '{}' for {} ({}): {}",
                            asset.asset_name,
                            tag,
                            asset.path.display(),
                            error
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// Build a release body by extracting the matching section from the crate's CHANGELOG.md
fn build_release_body_from_changelog(workspace: &Path, tag: &str) -> Option<String> {
    let config = SampoConfig::load(workspace).ok();
    let (crate_name, version) = parse_tag_with_config(tag, config.as_ref())?;

    // Find crate directory by name using the workspace API
    let ws = discover_workspace(workspace).ok()?;
    let crate_dir = ws
        .members
        .iter()
        .find(|c| c.name == crate_name)
        .map(|c| c.path.clone())
        // Fallback to a conventional path if discovery failed to find it
        .unwrap_or_else(|| workspace.join("crates").join(&crate_name));

    let changelog = crate_dir.join("CHANGELOG.md");
    extract_changelog_section(&changelog, &version)
}

/// Parse a tag through the configured templates (or the default templates
/// when no config is available).
fn parse_tag_with_config(tag: &str, config: Option<&SampoConfig>) -> Option<(String, String)> {
    match config {
        Some(cfg) => cfg.parse_tag(tag),
        None => SampoConfig::default().parse_tag(tag),
    }
}

fn tag_is_prerelease_with_config(tag: &str, config: Option<&SampoConfig>) -> bool {
    parse_tag_with_config(tag, config)
        .and_then(|(_name, version)| Version::parse(&version).ok())
        .map(|parsed| !parsed.pre.is_empty())
        .unwrap_or_else(|| {
            // Fallback: try to parse version directly from short tag format
            if tag.starts_with('v') {
                Version::parse(tag.trim_start_matches('v'))
                    .map(|v| !v.pre.is_empty())
                    .unwrap_or(false)
            } else {
                false
            }
        })
}

/// Extract the section that follows the first `##` heading until the next `##` or EOF.
///
/// The leading heading itself is stripped because the GitHub release title already
/// conveys the version.
fn extract_changelog_section(path: &Path, _version: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut collecting = false;
    let mut collected = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("## ") {
            if collecting {
                break;
            }
            collecting = true;
            continue;
        }

        if collecting {
            collected.push(line);
        }
    }

    let body = collected.join("\n").trim().to_string();
    if body.is_empty() { None } else { Some(body) }
}

fn parse_asset_specs(input: Option<&str>) -> Vec<AssetSpec> {
    input
        .map(|raw| {
            raw.lines()
                .flat_map(|line| line.split(','))
                .filter_map(|entry| {
                    let trimmed = entry.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    let mut parts = trimmed.splitn(2, "=>");
                    let pattern = parts.next().unwrap().trim();
                    if pattern.is_empty() {
                        return None;
                    }
                    let rename = parts
                        .next()
                        .map(|r| r.trim().to_string())
                        .filter(|r| !r.is_empty());
                    Some(AssetSpec {
                        pattern: pattern.to_string(),
                        rename,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_release_assets(
    workspace: &Path,
    tag: &str,
    specs: &[AssetSpec],
) -> Result<Vec<ResolvedAsset>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }

    let config = SampoConfig::load(workspace).ok();
    let parsed_tag = parse_tag_with_config(tag, config.as_ref());
    let crate_name = parsed_tag.as_ref().map(|(name, _)| name.as_str());
    let version = parsed_tag.as_ref().map(|(_, ver)| ver.as_str());

    let mut resolved = Vec::new();
    let mut used_names = BTreeSet::new();

    for spec in specs {
        let rendered_pattern = render_asset_template(&spec.pattern, tag, crate_name, version);
        let rename_template = spec
            .rename
            .as_deref()
            .map(|value| render_asset_template(value, tag, crate_name, version));

        let pattern_path = {
            let base = Path::new(&rendered_pattern);
            if base.is_absolute() {
                base.to_path_buf()
            } else {
                workspace.join(base)
            }
        };
        let pattern_string = pattern_path.to_string_lossy().into_owned();

        let entries = glob(&pattern_string).map_err(|e| ActionError::SampoCommandFailed {
            operation: "release-asset-discovery".to_string(),
            message: format!(
                "Invalid release asset pattern '{}': {}",
                rendered_pattern, e
            ),
        })?;

        let mut matched = false;
        for entry in entries {
            match entry {
                Ok(path) => {
                    if path.is_dir() {
                        println!(
                            "Skipping directory match for pattern '{}': {}",
                            spec.pattern,
                            path.display()
                        );
                        continue;
                    }

                    matched = true;
                    let asset_name = rename_template
                        .as_deref()
                        .map(|name| name.to_string())
                        .unwrap_or_else(|| {
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or(tag)
                                .to_string()
                        });

                    if asset_name.trim().is_empty() {
                        println!(
                            "Skipping asset with empty name for pattern '{}' (path: {})",
                            spec.pattern,
                            path.display()
                        );
                        continue;
                    }

                    if !used_names.insert(asset_name.clone()) {
                        println!(
                            "Skipping asset '{}' because another file already uses that name",
                            asset_name
                        );
                        continue;
                    }

                    resolved.push(ResolvedAsset { path, asset_name });
                }
                Err(err) => {
                    println!(
                        "Warning: Failed to read a path for pattern '{}': {}",
                        rendered_pattern, err
                    );
                }
            }
        }

        if !matched {
            println!(
                "No files matched release asset pattern '{}' (rendered: '{}')",
                spec.pattern, rendered_pattern
            );
        }
    }

    Ok(resolved)
}

fn render_asset_template(
    template: &str,
    tag: &str,
    crate_name: Option<&str>,
    version: Option<&str>,
) -> String {
    let mut rendered = template.replace("{{tag}}", tag);
    if let Some(name) = crate_name {
        rendered = rendered.replace("{{crate}}", name);
    } else {
        rendered = rendered.replace("{{crate}}", "");
    }
    if let Some(ver) = version {
        rendered = rendered.replace("{{version}}", ver);
    } else {
        rendered = rendered.replace("{{version}}", "");
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag_is_prerelease(tag: &str) -> bool {
        tag_is_prerelease_with_config(tag, None)
    }

    #[test]
    fn default_branch_slugifies_path_segments() {
        assert_eq!(default_pr_branch("main", false), "release/main");
        assert_eq!(default_pr_branch("3.x", false), "release/3.x");
        assert_eq!(
            default_pr_branch("feature/foo", false),
            "release/feature-foo"
        );
    }

    #[test]
    fn default_title_includes_branch_name() {
        assert_eq!(default_pr_title("main", false), "Release (main)");
        assert_eq!(default_pr_title("3.x", false), "Release (3.x)");
    }

    #[test]
    fn stabilize_branch_uses_dedicated_prefix() {
        assert_eq!(default_stabilize_pr_branch("main"), "stabilize/main");
        assert_eq!(
            default_stabilize_pr_branch("feature/foo"),
            "stabilize/feature-foo"
        );
    }

    #[test]
    fn stabilize_title_includes_branch_name() {
        assert_eq!(default_stabilize_pr_title("main"), "Release stable (main)");
        assert_eq!(
            default_stabilize_pr_title("release-branch"),
            "Release stable (release-branch)"
        );
    }

    #[test]
    fn collect_prerelease_packages_detects_members() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        // Create .sampo/ directory (required for discover_workspace)
        fs::create_dir_all(root.join(".sampo")).unwrap();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/foo\", \"crates/bar\"]\n",
        )
        .unwrap();

        let foo_dir = root.join("crates/foo");
        let bar_dir = root.join("crates/bar");
        fs::create_dir_all(&foo_dir).unwrap();
        fs::create_dir_all(&bar_dir).unwrap();

        fs::write(
            foo_dir.join("Cargo.toml"),
            "[package]\nname=\"foo\"\nversion=\"0.2.0-beta.3\"\n",
        )
        .unwrap();

        fs::write(
            bar_dir.join("Cargo.toml"),
            "[package]\nname=\"bar\"\nversion=\"1.4.0\"\n",
        )
        .unwrap();

        let packages = collect_prerelease_packages(root).unwrap();
        assert_eq!(packages, vec!["cargo/foo".to_string()]);
    }

    #[test]
    fn collect_prerelease_packages_disambiguates_same_name() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        fs::create_dir_all(root.join(".sampo")).unwrap();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/foo\"]\n",
        )
        .unwrap();

        let foo_dir = root.join("crates/foo");
        fs::create_dir_all(&foo_dir).unwrap();

        fs::write(
            foo_dir.join("Cargo.toml"),
            "[package]\nname=\"foo\"\nversion=\"0.2.0-beta.3\"\n",
        )
        .unwrap();

        fs::write(
            root.join("package.json"),
            r#"{"name":"foo","version":"1.0.0-rc.1","workspaces":["packages/*"]}"#,
        )
        .unwrap();

        let packages = collect_prerelease_packages(root).unwrap();

        assert!(packages.contains(&"cargo/foo".to_string()));
        assert!(packages.contains(&"npm/foo".to_string()));
        assert_eq!(packages.len(), 2);
    }

    #[test]
    fn pre_release_branch_has_dedicated_prefix_and_title() {
        assert_eq!(default_pr_branch("next", true), "pre-release/next");
        assert_eq!(default_pr_title("next", true), "Pre-release (next)");
    }

    #[test]
    fn tag_pre_release_detection() {
        assert!(tag_is_prerelease("sampo-v1.2.0-alpha.1"));
        assert!(!tag_is_prerelease("sampo-v1.2.0"));
        assert!(!tag_is_prerelease("invalid"));
    }

    #[test]
    fn test_mode_parsing() {
        assert!(matches!(Mode::parse("auto"), Mode::Auto));
        assert!(matches!(Mode::parse("release"), Mode::Release));
        assert!(matches!(Mode::parse("publish"), Mode::Publish));
        assert!(matches!(Mode::parse("unknown"), Mode::Auto));
    }

    #[test]
    fn test_determine_workspace_with_config_override() {
        let config = Config {
            working_directory: Some(PathBuf::from("/test/path")),
            mode: Mode::Auto,
            dry_run: false,
            cargo_token: None,
            args: None,
            cargo_args: None,
            npm_args: None,
            hex_args: None,
            pypi_args: None,
            packagist_args: None,
            maven_args: None,
            base_branch: None,
            pr_branch: None,
            pr_title: None,
            stabilize_pr_branch: None,
            stabilize_pr_title: None,
            create_forgejo_release: false,
            release_assets: None,
        };
        let result = determine_workspace(&config).unwrap();
        assert_eq!(result, PathBuf::from("/test/path"));
    }

    #[test]
    fn test_create_github_client() {
        // Test without environment variables
        unsafe {
            std::env::remove_var("GITHUB_REPOSITORY");
            std::env::remove_var("GITHUB_TOKEN");
        }
        assert!(create_forgejo_client().is_err());

        // Test with empty values
        unsafe {
            std::env::set_var("GITHUB_REPOSITORY", "");
            std::env::set_var("GITHUB_TOKEN", "token");
        }
        assert!(create_forgejo_client().is_err());

        // Test with valid values
        unsafe {
            std::env::set_var("GITHUB_REPOSITORY", "owner/repo");
            std::env::set_var("GITHUB_TOKEN", "valid_token");
        }
        assert!(create_forgejo_client().is_ok());

        // Clean up
        unsafe {
            std::env::remove_var("GITHUB_REPOSITORY");
            std::env::remove_var("GITHUB_TOKEN");
        }
    }

    #[test]
    fn test_emit_github_output() {
        // This test would need mocking for real testing
        assert!(emit_forgejo_output("test", true).is_ok());
    }

    #[test]
    fn parse_asset_specs_supports_patterns_and_rename() {
        let specs = parse_asset_specs(Some("dist/*.zip => package.zip,extra.bin"));
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].pattern, "dist/*.zip");
        assert_eq!(specs[0].rename.as_deref(), Some("package.zip"));
        assert_eq!(specs[1].pattern, "extra.bin");
        assert!(specs[1].rename.is_none());
    }

    #[test]
    fn render_asset_template_substitutes_known_placeholders() {
        let rendered = render_asset_template(
            "artifacts/{{crate}}-{{version}}/{{tag}}.tar.gz",
            "my-crate-v1.0.0",
            Some("my-crate"),
            Some("1.0.0"),
        );
        assert_eq!(rendered, "artifacts/my-crate-1.0.0/my-crate-v1.0.0.tar.gz");
    }

    #[test]
    fn resolve_release_assets_matches_files() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        let dist_dir = workspace.join("dist");
        fs::create_dir_all(&dist_dir).unwrap();
        let artifact_path = dist_dir.join("my-crate-v1.0.0-x86_64.tar.gz");
        fs::write(&artifact_path, b"dummy").unwrap();

        let specs = vec![AssetSpec {
            pattern: "dist/{{crate}}-v{{version}}-*.tar.gz".to_string(),
            rename: Some("{{crate}}-{{version}}.tar.gz".to_string()),
        }];

        let assets = resolve_release_assets(workspace, "my-crate-v1.0.0", &specs)
            .expect("asset resolution should succeed");
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].asset_name, "my-crate-1.0.0.tar.gz");
        assert_eq!(assets[0].path, artifact_path);
    }

    #[test]
    fn parse_tag_with_default_template() {
        assert_eq!(
            parse_tag_with_config("my-crate-v1.2.3", None),
            Some(("my-crate".to_string(), "1.2.3".to_string()))
        );
        assert_eq!(
            parse_tag_with_config("sampo-v0.9.0", None),
            Some(("sampo".to_string(), "0.9.0".to_string()))
        );
        assert_eq!(
            parse_tag_with_config("sampo-github-action-v0.8.2", None),
            Some(("sampo-github-action".to_string(), "0.8.2".to_string()))
        );
        assert_eq!(parse_tag_with_config("nope", None), None);
        // Short tags require explicit `git.short_tags` config; under the
        // default template a bare `v…` doesn't match the `{package_name}` slot.
        assert_eq!(parse_tag_with_config("v1.2.3", None), None);
    }

    #[test]
    fn parse_tag_with_config_handles_short_format() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".sampo")).unwrap();
        fs::write(
            temp.path().join(".sampo/config.toml"),
            "[git]\nshort_tags = \"my-package\"\n",
        )
        .unwrap();

        let config = SampoConfig::load(temp.path()).unwrap();

        assert_eq!(
            parse_tag_with_config("v1.2.3", Some(&config)),
            Some(("my-package".to_string(), "1.2.3".to_string()))
        );

        assert_eq!(
            parse_tag_with_config("v1.2.3-alpha.1", Some(&config)),
            Some(("my-package".to_string(), "1.2.3-alpha.1".to_string()))
        );

        assert_eq!(
            parse_tag_with_config("other-package-v2.0.0", Some(&config)),
            Some(("other-package".to_string(), "2.0.0".to_string()))
        );

        assert_eq!(parse_tag_with_config("v1.2", Some(&config)), None);
        assert_eq!(parse_tag_with_config("vfoo", Some(&config)), None);
    }

    #[test]
    fn parse_tag_honours_custom_tag_format() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".sampo")).unwrap();
        fs::write(
            temp.path().join(".sampo/config.toml"),
            "[git]\ntag_format = \"{ecosystem}-{package_name}-v{version}\"\n",
        )
        .unwrap();

        let config = SampoConfig::load(temp.path()).unwrap();
        assert_eq!(
            parse_tag_with_config("cargo-my-crate-v1.2.3", Some(&config)),
            Some(("my-crate".to_string(), "1.2.3".to_string()))
        );
        // Default-shape tags no longer match once the user opts into the
        // disambiguating template.
        assert_eq!(
            parse_tag_with_config("my-crate-v1.2.3", Some(&config)),
            None
        );
    }

    #[test]
    fn tag_is_prerelease_with_config_detects_short_format_prereleases() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".sampo")).unwrap();
        fs::write(
            temp.path().join(".sampo/config.toml"),
            "[git]\nshort_tags = \"my-package\"\n",
        )
        .unwrap();

        let config = SampoConfig::load(temp.path()).unwrap();

        assert!(tag_is_prerelease_with_config(
            "v1.0.0-alpha.1",
            Some(&config)
        ));
        assert!(tag_is_prerelease_with_config("v2.0.0-beta", Some(&config)));
        assert!(tag_is_prerelease_with_config("v1.0.0-rc.1", Some(&config)));

        assert!(!tag_is_prerelease_with_config("v1.0.0", Some(&config)));
        assert!(!tag_is_prerelease_with_config("v2.3.4", Some(&config)));

        assert!(tag_is_prerelease_with_config(
            "other-v1.0.0-alpha.1",
            Some(&config)
        ));
        assert!(!tag_is_prerelease_with_config(
            "other-v1.0.0",
            Some(&config)
        ));
    }

    #[test]
    fn resolve_release_assets_with_short_tags() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();

        fs::create_dir_all(workspace.join(".sampo")).unwrap();
        fs::write(
            workspace.join(".sampo/config.toml"),
            "[git]\nshort_tags = \"my-package\"\n",
        )
        .unwrap();

        let dist_dir = workspace.join("dist");
        fs::create_dir_all(&dist_dir).unwrap();
        let artifact_path = dist_dir.join("my-package-1.0.0-x86_64.tar.gz");
        fs::write(&artifact_path, b"dummy").unwrap();

        let specs = vec![AssetSpec {
            pattern: "dist/{{crate}}-{{version}}-*.tar.gz".to_string(),
            rename: Some("{{crate}}-{{version}}-release.tar.gz".to_string()),
        }];

        // Short tag format: v1.0.0 -> crate=my-package, version=1.0.0
        let assets = resolve_release_assets(workspace, "v1.0.0", &specs)
            .expect("asset resolution should succeed with short tags");

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].asset_name, "my-package-1.0.0-release.tar.gz");
        assert_eq!(assets[0].path, artifact_path);
    }

    #[test]
    fn resolve_release_assets_short_tags_with_prerelease() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();

        fs::create_dir_all(workspace.join(".sampo")).unwrap();
        fs::write(
            workspace.join(".sampo/config.toml"),
            "[git]\nshort_tags = \"my-package\"\n",
        )
        .unwrap();

        let dist_dir = workspace.join("dist");
        fs::create_dir_all(&dist_dir).unwrap();
        let artifact_path = dist_dir.join("my-package-1.0.0-alpha.1-linux.tar.gz");
        fs::write(&artifact_path, b"dummy").unwrap();

        let specs = vec![AssetSpec {
            pattern: "dist/{{crate}}-{{version}}-linux.tar.gz".to_string(),
            rename: None,
        }];

        let assets = resolve_release_assets(workspace, "v1.0.0-alpha.1", &specs)
            .expect("asset resolution should succeed with short tag prerelease");

        assert_eq!(assets.len(), 1);
        // When no rename, asset_name is the original filename
        assert_eq!(
            assets[0].asset_name,
            "my-package-1.0.0-alpha.1-linux.tar.gz"
        );
    }

    #[test]
    fn test_asset_filtering_per_crate() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        let dist_dir = workspace.join("dist");
        fs::create_dir_all(&dist_dir).unwrap();

        // Create binaries for both sampo and sampo-github-action
        fs::write(
            dist_dir.join("sampo-x86_64-unknown-linux-gnu.tar.gz"),
            b"sampo-linux",
        )
        .unwrap();
        fs::write(
            dist_dir.join("sampo-x86_64-apple-darwin.tar.gz"),
            b"sampo-macos",
        )
        .unwrap();
        fs::write(
            dist_dir.join("sampo-github-action-x86_64-unknown-linux-gnu.tar.gz"),
            b"action-linux",
        )
        .unwrap();
        fs::write(
            dist_dir.join("sampo-github-action-x86_64-apple-darwin.tar.gz"),
            b"action-macos",
        )
        .unwrap();

        // Asset specs using {{crate}} template
        let specs = vec![
            AssetSpec {
                pattern: "dist/{{crate}}-x86_64-unknown-linux-gnu.tar.gz".to_string(),
                rename: Some("{{crate}}-{{version}}-x86_64-unknown-linux-gnu.tar.gz".to_string()),
            },
            AssetSpec {
                pattern: "dist/{{crate}}-x86_64-apple-darwin.tar.gz".to_string(),
                rename: Some("{{crate}}-{{version}}-x86_64-apple-darwin.tar.gz".to_string()),
            },
        ];

        // Test with sampo tag - should only match sampo binaries
        let sampo_assets = resolve_release_assets(workspace, "sampo-v0.9.0", &specs)
            .expect("sampo asset resolution should succeed");
        assert_eq!(
            sampo_assets.len(),
            2,
            "Should find exactly 2 sampo binaries"
        );
        assert!(
            sampo_assets
                .iter()
                .all(|a| a.asset_name.starts_with("sampo-0.9.0-"))
        );
        assert!(sampo_assets.iter().any(|a| a.asset_name.contains("linux")));
        assert!(sampo_assets.iter().any(|a| a.asset_name.contains("darwin")));

        // Test with sampo-github-action tag - should only match action binaries
        let action_assets = resolve_release_assets(workspace, "sampo-github-action-v0.8.2", &specs)
            .expect("action asset resolution should succeed");
        assert_eq!(
            action_assets.len(),
            2,
            "Should find exactly 2 action binaries"
        );
        assert!(
            action_assets
                .iter()
                .all(|a| a.asset_name.starts_with("sampo-github-action-0.8.2-"))
        );
        assert!(action_assets.iter().any(|a| a.asset_name.contains("linux")));
        assert!(
            action_assets
                .iter()
                .any(|a| a.asset_name.contains("darwin"))
        );
    }

    #[test]
    fn test_extract_changelog_section_with_timestamp() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CHANGELOG.md");
        let content = "# my-crate\n\n## 1.2.3 — 2024-06-17\n\n### Patch changes\n\n- Fix: foo\n\n## 1.2.2\n\n- Older";
        fs::write(&file, content).unwrap();

        let got = extract_changelog_section(&file, "1.2.3").unwrap();
        assert!(got.starts_with("### Patch changes"));
        assert!(!got.contains("## 1.2.3"));
        assert!(got.contains("Fix: foo"));
        assert!(!got.contains("1.2.2"));
    }

    #[test]
    fn test_extract_changelog_section_without_timestamp() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("CHANGELOG.md");
        let content =
            "# my-crate\n\n## 2.0.0\n\n- New feature\n\n## 1.9.0 — 2023-12-01\n\n- Previous";
        fs::write(&file, content).unwrap();

        let got = extract_changelog_section(&file, "2.0.0").unwrap();
        assert!(got.starts_with("- New feature"));
        assert!(!got.contains("## 2.0.0"));
        assert!(!got.contains("1.9.0"));
    }
}

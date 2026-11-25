use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zip::ZipArchive;

use crate::manifest::{ManifestError, PluginManifest, effective_host_version, is_valid_plugin_id};

/// File recorded inside an installed GitHub plugin directory tracking its originating repo/version.
pub const ORIGIN_FILE: &str = ".origin.json";

/// Persisted origin metadata recorded inside GitHub-installed plugins (`{"repo":"owner/repo","version":"..."}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginOrigin {
    pub repo: String,
    pub version: String,
}

/// Maximum uncompressed bytes extracted from a plugin ZIP archive (64 MiB).
const MAX_ZIP_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum number of entries allowed in a plugin ZIP archive.
const MAX_ZIP_ENTRIES: usize = 2048;

/// Errors returned by [`PluginInstaller`].
#[derive(Debug, Error)]
pub enum InstallerError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("invalid ZIP archive: {0}")]
    Zip(String),
    #[error("security violation: zip-slip path traversal detected in entry `{0}`")]
    ZipSlip(String),
    #[error("security violation: symlinks are not permitted in plugin archive (`{0}`)")]
    SymlinkNotAllowed(String),
    #[error("archive exceeds safety limits: {0}")]
    ArchiveLimit(String),
    #[error("invalid GitHub repository specification `{0}`")]
    InvalidGitHubSpec(String),
    #[error("HTTP download failed: {0}")]
    Http(String),
    #[error("plugin `{0}` is not installed")]
    NotInstalled(String),
    #[error("invalid plugin ID `{0}`")]
    InvalidPluginId(String),
    #[error(
        "a plugin called `{plugin_id}` is already installed from a different origin (`{existing_repo}` vs `{incoming_repo}`)"
    )]
    OriginConflict {
        plugin_id: String,
        existing_repo: String,
        incoming_repo: String,
    },
    #[error("registry serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Origin of an installed plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InstallSource {
    LocalDir {
        path: String,
    },
    ZipArchive {
        path: String,
    },
    GitHubRepo {
        owner: String,
        repo: String,
        git_ref: String,
    },
    Discovered,
}

/// Inspected & staged plugin ready for user confirmation (manifest + permissions preview).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPluginInstall {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub source: InstallSource,
    pub staging_dir: PathBuf,
    pub plugin_root: PathBuf,
    pub network_hosts: Vec<String>,
    pub process_commands: Vec<String>,
    pub is_update: bool,
}

/// Persisted metadata for an installed plugin in `registry.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPluginMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub granted: bool,
    pub source: InstallSource,
    pub dir: PathBuf,
    pub installed_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PluginRegistry {
    plugins: Vec<InstalledPluginMeta>,
}

/// Parsed GitHub repository target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepoSpec {
    pub owner: String,
    pub repo: String,
    pub git_ref: Option<String>,
}

impl GitHubRepoSpec {
    /// Parses `owner/repo`, `owner/repo@ref`, `git@github.com:owner/repo.git`, `github.com/owner/repo`,
    /// `https://github.com/owner/repo`, or `https://github.com/owner/repo/tree/<ref>`.
    pub fn parse(input: &str) -> Result<Self, InstallerError> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err(InstallerError::InvalidGitHubSpec(input.to_owned()));
        }

        // Handle SSH clone specs (`git@github.com:owner/repo.git`).
        if let Some(ssh_rest) = strip_prefix_ignore_ascii_case(raw, "git@github.com:") {
            let clean = ssh_rest
                .split(['?', '#'])
                .next()
                .unwrap_or(ssh_rest)
                .trim_end_matches('/')
                .trim_end_matches(".git");
            return Self::parse_slug(clean, None, input);
        }

        // Handle full HTTP(S) GitHub URLs or bare `github.com/owner/repo` URLs.
        let lower = raw.to_ascii_lowercase();
        let maybe_url = if lower.starts_with("http://") || lower.starts_with("https://") {
            Some(raw.to_owned())
        } else if lower.starts_with("github.com/") || lower.starts_with("www.github.com/") {
            Some(format!("https://{raw}"))
        } else {
            None
        };

        if let Some(url_str) = maybe_url {
            let parsed = url::Url::parse(&url_str)
                .map_err(|_| InstallerError::InvalidGitHubSpec(input.to_owned()))?;
            let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
            if host != "github.com" && host != "www.github.com" {
                return Err(InstallerError::InvalidGitHubSpec(input.to_owned()));
            }
            let segments: Vec<&str> = parsed
                .path_segments()
                .map(|s| s.filter(|p| !p.is_empty()).collect())
                .unwrap_or_default();
            if segments.len() < 2 {
                return Err(InstallerError::InvalidGitHubSpec(input.to_owned()));
            }
            let owner = segments[0];
            let repo = segments[1].trim_end_matches(".git");
            let git_ref = if segments.len() >= 4
                && matches!(segments[2], "tree" | "blob" | "releases")
                && segments[2] != "releases"
            {
                Some(segments[3..].join("/"))
            } else if segments.len() >= 5 && segments[2] == "releases" && segments[3] == "tag" {
                Some(segments[4..].join("/"))
            } else {
                None
            };
            validate_gh_segment(owner, input)?;
            validate_gh_segment(repo, input)?;
            return Ok(Self {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                git_ref,
            });
        }

        // Handle shorthand `owner/repo` or `owner/repo@ref`.
        let (slug, git_ref) = match raw.split_once('@') {
            Some((left, right)) if !right.trim().is_empty() => {
                (left, Some(right.trim().to_owned()))
            }
            _ => (raw, None),
        };
        Self::parse_slug(slug, git_ref, input)
    }

    fn parse_slug(
        slug: &str,
        git_ref: Option<String>,
        original: &str,
    ) -> Result<Self, InstallerError> {
        let parts: Vec<&str> = slug.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() != 2 {
            return Err(InstallerError::InvalidGitHubSpec(original.to_owned()));
        }
        let owner = parts[0].to_owned();
        let repo = parts[1].trim_end_matches(".git").to_owned();
        validate_gh_segment(&owner, original)?;
        validate_gh_segment(&repo, original)?;
        Ok(Self {
            owner,
            repo,
            git_ref,
        })
    }

    /// Candidate archive download URLs in preference order.
    pub fn archive_urls(&self) -> Vec<(String, String)> {
        if let Some(r) = &self.git_ref {
            vec![
                (
                    r.clone(),
                    format!(
                        "https://codeload.github.com/{}/{}/zip/{}",
                        self.owner, self.repo, r
                    ),
                ),
                (
                    r.clone(),
                    format!(
                        "https://codeload.github.com/{}/{}/zip/refs/tags/{}",
                        self.owner, self.repo, r
                    ),
                ),
                (
                    r.clone(),
                    format!(
                        "https://codeload.github.com/{}/{}/zip/refs/heads/{}",
                        self.owner, self.repo, r
                    ),
                ),
                (
                    r.clone(),
                    format!(
                        "https://github.com/{}/{}/archive/{}.zip",
                        self.owner, self.repo, r
                    ),
                ),
            ]
        } else {
            vec![
                (
                    "main".to_owned(),
                    format!(
                        "https://codeload.github.com/{}/{}/zip/refs/heads/main",
                        self.owner, self.repo
                    ),
                ),
                (
                    "master".to_owned(),
                    format!(
                        "https://codeload.github.com/{}/{}/zip/refs/heads/master",
                        self.owner, self.repo
                    ),
                ),
                (
                    "main".to_owned(),
                    format!(
                        "https://github.com/{}/{}/archive/refs/heads/main.zip",
                        self.owner, self.repo
                    ),
                ),
                (
                    "master".to_owned(),
                    format!(
                        "https://github.com/{}/{}/archive/refs/heads/master.zip",
                        self.owner, self.repo
                    ),
                ),
            ]
        }
    }
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn validate_gh_segment(seg: &str, original: &str) -> Result<(), InstallerError> {
    if seg.is_empty()
        || seg == "."
        || seg == ".."
        || !seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(InstallerError::InvalidGitHubSpec(original.to_owned()));
    }
    Ok(())
}

/// Manages installing, uninstalling, enabling, disabling, and discovering plugins on disk.
#[derive(Debug, Clone)]
pub struct PluginInstaller {
    plugins_dir: PathBuf,
    registry_path: PathBuf,
}

impl PluginInstaller {
    /// Creates an installer managing `plugins_dir` (created if missing).
    pub fn new(plugins_dir: impl Into<PathBuf>) -> Result<Self, InstallerError> {
        let plugins_dir = plugins_dir.into();
        fs::create_dir_all(&plugins_dir).map_err(|source| InstallerError::Io {
            path: plugins_dir.clone(),
            source,
        })?;
        let registry_path = plugins_dir.join("registry.json");
        Ok(Self {
            plugins_dir,
            registry_path,
        })
    }

    /// Root directory where installed plugins reside.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    /// Installs or updates a plugin from a local directory containing `plugin.json`.
    pub fn install_from_dir(&self, src_dir: &Path) -> Result<InstalledPluginMeta, InstallerError> {
        let resolved_src = locate_plugin_root(src_dir)?;
        let manifest = PluginManifest::from_dir(&resolved_src)?;
        manifest.check_min_app_version(&effective_host_version())?;
        let _ = manifest.resolve_entry_path(&resolved_src)?;

        let staging = self.create_staging_dir(&manifest.id)?;
        let copy_res = copy_dir_recursive(&resolved_src, &staging);
        if let Err(err) = copy_res {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }

        let res = self.finalize_install(
            &manifest,
            &staging,
            InstallSource::LocalDir {
                path: src_dir.display().to_string(),
            },
        );
        self.cleanup_staging_dir(&staging);
        res
    }

    /// Installs or updates a plugin from a `.zip` file on disk (with zip-slip protection).
    pub fn install_from_zip(&self, zip_path: &Path) -> Result<InstalledPluginMeta, InstallerError> {
        let bytes = fs::read(zip_path).map_err(|source| InstallerError::Io {
            path: zip_path.to_path_buf(),
            source,
        })?;
        self.install_from_zip_bytes(
            &bytes,
            InstallSource::ZipArchive {
                path: zip_path.display().to_string(),
            },
        )
    }

    /// Installs or updates a plugin from raw `.zip` bytes (with strict zip-slip & zip-bomb protection).
    pub fn install_from_zip_bytes(
        &self,
        bytes: &[u8],
        source: InstallSource,
    ) -> Result<InstalledPluginMeta, InstallerError> {
        let staging = self.create_staging_dir("zip-extract")?;
        let extract_res = extract_zip_safely(bytes, &staging);
        if let Err(err) = extract_res {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }

        let plugin_root = match locate_plugin_root(&staging) {
            Ok(root) => root,
            Err(err) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(err);
            }
        };
        let manifest = match PluginManifest::from_dir(&plugin_root) {
            Ok(m) => m,
            Err(err) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(InstallerError::Manifest(err));
            }
        };
        if let Err(err) = manifest.check_min_app_version(&effective_host_version()) {
            let _ = fs::remove_dir_all(&staging);
            return Err(InstallerError::Manifest(err));
        }
        if let Err(err) = manifest.resolve_entry_path(&plugin_root) {
            let _ = fs::remove_dir_all(&staging);
            return Err(InstallerError::Manifest(err));
        }

        let res = self.finalize_install(&manifest, &plugin_root, source);
        let _ = fs::remove_dir_all(&staging);
        res
    }

    /// Downloads and installs a plugin from GitHub (`owner/repo`, `owner/repo@ref`, or GitHub URL).
    pub fn install_from_github(&self, spec: &str) -> Result<InstalledPluginMeta, InstallerError> {
        let pending = self.stage_from_github(spec)?;
        self.confirm_staged(&pending, true)
    }

    /// Inspects and stages a local plugin directory for confirmation before installing.
    pub fn stage_from_dir(&self, src_dir: &Path) -> Result<PendingPluginInstall, InstallerError> {
        let resolved_src = locate_plugin_root(src_dir)?;
        let manifest = PluginManifest::from_dir(&resolved_src)?;
        manifest.check_min_app_version(&effective_host_version())?;
        let _ = manifest.resolve_entry_path(&resolved_src)?;

        let staging = self.create_staging_dir(&format!("preview-{}", manifest.id))?;
        if let Err(err) = copy_dir_recursive(&resolved_src, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }

        self.build_pending_install(
            &manifest,
            staging.clone(),
            staging,
            InstallSource::LocalDir {
                path: src_dir.display().to_string(),
            },
        )
    }

    /// Inspects and stages a `.zip` plugin archive for confirmation before installing.
    pub fn stage_from_zip(&self, zip_path: &Path) -> Result<PendingPluginInstall, InstallerError> {
        let bytes = fs::read(zip_path).map_err(|source| InstallerError::Io {
            path: zip_path.to_path_buf(),
            source,
        })?;
        self.stage_from_zip_bytes(
            &bytes,
            InstallSource::ZipArchive {
                path: zip_path.display().to_string(),
            },
        )
    }

    /// Inspects and stages raw `.zip` bytes for confirmation before installing.
    pub fn stage_from_zip_bytes(
        &self,
        bytes: &[u8],
        source: InstallSource,
    ) -> Result<PendingPluginInstall, InstallerError> {
        let staging = self.create_staging_dir("preview-zip")?;
        if let Err(err) = extract_zip_safely(bytes, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }

        let plugin_root = match locate_plugin_root(&staging) {
            Ok(root) => root,
            Err(err) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(err);
            }
        };
        let manifest = match PluginManifest::from_dir(&plugin_root) {
            Ok(m) => m,
            Err(err) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(InstallerError::Manifest(err));
            }
        };
        if let Err(err) = manifest.check_min_app_version(&effective_host_version()) {
            let _ = fs::remove_dir_all(&staging);
            return Err(InstallerError::Manifest(err));
        }
        if let Err(err) = manifest.resolve_entry_path(&plugin_root) {
            let _ = fs::remove_dir_all(&staging);
            return Err(InstallerError::Manifest(err));
        }

        self.build_pending_install(&manifest, staging, plugin_root, source)
    }

    /// Downloads and stages a GitHub plugin repository for confirmation before installing.
    pub fn stage_from_github(&self, spec: &str) -> Result<PendingPluginInstall, InstallerError> {
        let parsed = GitHubRepoSpec::parse(spec)?;
        let spec_for_task = parsed.clone();

        let (used_ref, zip_bytes) = run_future_blocking(async move {
            let client = build_installer_http_client(Duration::from_secs(60))?;
            let mut candidates = Vec::new();

            if spec_for_task.git_ref.is_none() {
                // 1. Prefer latest release asset (or release zipball) when published.
                match fetch_latest_release_archive(
                    &client,
                    &spec_for_task.owner,
                    &spec_for_task.repo,
                )
                .await
                {
                    ReleaseLookup::Found(tag, rel_url) => {
                        candidates.push((tag, rel_url));
                    }
                    ReleaseLookup::NotFound => {
                        // 2. When `/releases/latest` returns 404, fall back to `/commits/HEAD` + zipball.
                        if let Some((short_sha, zipball_url)) = fetch_head_commit_archive(
                            &client,
                            &spec_for_task.owner,
                            &spec_for_task.repo,
                        )
                        .await
                        {
                            candidates.push((short_sha, zipball_url));
                        }
                    }
                    ReleaseLookup::Error => {}
                }
                // 3. Also inspect repository default branch in case it isn't `main` or `master`.
                if let Some(default_branch) =
                    fetch_default_branch(&client, &spec_for_task.owner, &spec_for_task.repo).await
                {
                    candidates.push((
                        default_branch.clone(),
                        format!(
                            "https://codeload.github.com/{}/{}/zip/refs/heads/{}",
                            spec_for_task.owner, spec_for_task.repo, default_branch
                        ),
                    ));
                }
            }

            for item in spec_for_task.archive_urls() {
                if !candidates.iter().any(|(_, u)| u == &item.1) {
                    candidates.push(item);
                }
            }

            let mut last_err = String::from("no candidate URLs");
            for (git_ref, url) in candidates {
                let accept = if url.contains("/releases/assets/") {
                    "application/octet-stream"
                } else if url.starts_with("https://api.github.com/") {
                    "application/vnd.github+json"
                } else {
                    ""
                };
                match github_send_with_retry(&client, &url, accept, Duration::from_secs(60)).await {
                    Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                        Ok(bytes) if bytes.starts_with(b"PK") => {
                            return Ok((git_ref, bytes.to_vec()));
                        }
                        Ok(bytes) => {
                            last_err = format!(
                                "{url}: downloaded payload ({} bytes) is not a valid ZIP archive",
                                bytes.len()
                            );
                        }
                        Err(e) => last_err = format!("{url}: {e}"),
                    },
                    Ok(resp) => {
                        last_err = format!("{url}: HTTP {}", resp.status());
                    }
                    Err(e) => {
                        last_err = format!("{url}: {e}");
                    }
                }
            }
            Err(InstallerError::Http(last_err))
        })?;

        self.stage_from_zip_bytes(
            &zip_bytes,
            InstallSource::GitHubRepo {
                owner: parsed.owner,
                repo: parsed.repo,
                git_ref: used_ref,
            },
        )
    }

    /// Finalizes a previously staged plugin installation and removes temporary staging files.
    pub fn confirm_staged(
        &self,
        pending: &PendingPluginInstall,
        granted: bool,
    ) -> Result<InstalledPluginMeta, InstallerError> {
        let manifest = PluginManifest::from_dir(&pending.plugin_root)?;
        manifest.check_min_app_version(&effective_host_version())?;
        let _ = manifest.resolve_entry_path(&pending.plugin_root)?;
        let res = self.finalize_install_with_grant(
            &manifest,
            &pending.plugin_root,
            pending.source.clone(),
            Some(granted),
        );
        self.cleanup_staging_dir(&pending.staging_dir);
        res
    }

    /// Discards a staged plugin installation without modifying installed plugins.
    pub fn cancel_staged(&self, pending: &PendingPluginInstall) {
        self.cleanup_staging_dir(&pending.staging_dir);
    }

    /// Checks whether a GitHub-installed plugin has a newer upstream version available.
    pub fn check_github_update(&self, plugin_id: &str) -> Result<Option<String>, InstallerError> {
        let reg = self.load_registry()?;
        let meta = reg
            .plugins
            .iter()
            .find(|p| p.id == plugin_id)
            .ok_or_else(|| InstallerError::NotInstalled(plugin_id.to_owned()))?;

        let InstallSource::GitHubRepo {
            owner,
            repo,
            git_ref,
        } = &meta.source
        else {
            return Ok(None);
        };

        let origin = read_plugin_origin(&meta.dir);
        let current_version = origin
            .as_ref()
            .filter(|o| !o.version.is_empty())
            .map(|o| o.version.clone())
            .unwrap_or_else(|| meta.version.clone());
        let owner = owner.clone();
        let repo = repo.clone();
        let git_ref = git_ref.clone();

        let remote_version = run_future_blocking(async move {
            let client = build_installer_http_client(Duration::from_secs(15))?;
            match fetch_latest_release_archive(&client, &owner, &repo).await {
                ReleaseLookup::Found(tag, _) => {
                    let clean = tag.trim().trim_start_matches('v').to_owned();
                    if !clean.is_empty() {
                        return Ok(Some(clean));
                    }
                }
                ReleaseLookup::NotFound => {
                    if let Some((short_sha, _)) =
                        fetch_head_commit_archive(&client, &owner, &repo).await
                    {
                        return Ok(Some(short_sha));
                    }
                }
                ReleaseLookup::Error => {}
            }

            let candidates = [
                format!("https://raw.githubusercontent.com/{owner}/{repo}/{git_ref}/plugin.json"),
                format!("https://raw.githubusercontent.com/{owner}/{repo}/main/plugin.json"),
                format!("https://raw.githubusercontent.com/{owner}/{repo}/master/plugin.json"),
            ];
            for url in candidates {
                if let Ok(resp) = client.get(&url).send().await
                    && resp.status().is_success()
                    && let Ok(text) = resp.text().await
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
                    && let Some(ver) = val.get("version").and_then(|v| v.as_str())
                {
                    return Ok(Some(ver.to_owned()));
                }
            }
            Ok(None)
        })?;

        Ok(remote_version.filter(|v| !v.is_empty() && is_newer_version(&current_version, v)))
    }

    /// Removes an installed plugin from disk and registry.
    pub fn uninstall(&self, plugin_id: &str) -> Result<(), InstallerError> {
        if !is_valid_plugin_id(plugin_id) {
            return Err(InstallerError::InvalidPluginId(plugin_id.to_owned()));
        }
        let target_dir = self.plugins_dir.join(plugin_id);
        let mut reg = self.load_registry()?;
        let had_entry = reg.plugins.iter().any(|p| p.id == plugin_id);
        reg.plugins.retain(|p| p.id != plugin_id);

        if target_dir.exists() {
            fs::remove_dir_all(&target_dir).map_err(|source| InstallerError::Io {
                path: target_dir.clone(),
                source,
            })?;
        } else if !had_entry {
            return Err(InstallerError::NotInstalled(plugin_id.to_owned()));
        }

        self.save_registry(&reg)?;
        Ok(())
    }

    /// Enables or disables an installed plugin in `registry.json`.
    pub fn set_enabled(
        &self,
        plugin_id: &str,
        enabled: bool,
    ) -> Result<InstalledPluginMeta, InstallerError> {
        let mut reg = self.load_registry()?;
        if let Some(item) = reg.plugins.iter_mut().find(|p| p.id == plugin_id) {
            item.enabled = enabled;
            let updated = item.clone();
            self.save_registry(&reg)?;
            return Ok(updated);
        }
        // Check if present on disk but not yet registered.
        let discovered = self.discover_plugins()?;
        if discovered.iter().any(|p| p.id == plugin_id) {
            let mut reg = self.load_registry()?;
            if let Some(item) = reg.plugins.iter_mut().find(|p| p.id == plugin_id) {
                item.enabled = enabled;
                let updated = item.clone();
                self.save_registry(&reg)?;
                return Ok(updated);
            }
        }
        Err(InstallerError::NotInstalled(plugin_id.to_owned()))
    }

    /// Updates user permission grant status for a plugin.
    pub fn set_granted(
        &self,
        plugin_id: &str,
        granted: bool,
    ) -> Result<InstalledPluginMeta, InstallerError> {
        let mut reg = self.load_registry()?;
        let Some(item) = reg.plugins.iter_mut().find(|p| p.id == plugin_id) else {
            return Err(InstallerError::NotInstalled(plugin_id.to_owned()));
        };
        item.granted = granted;
        let updated = item.clone();
        self.save_registry(&reg)?;
        Ok(updated)
    }

    /// Lists plugins recorded in `registry.json` (reconciled with disk via [`Self::discover_plugins`]).
    pub fn list_installed(&self) -> Result<Vec<InstalledPluginMeta>, InstallerError> {
        self.discover_plugins()
    }

    /// Discovers plugins on disk and synchronizes `registry.json`.
    pub fn discover_plugins(&self) -> Result<Vec<InstalledPluginMeta>, InstallerError> {
        let mut reg = self.load_registry()?;
        let mut changed = false;

        // Drop registry records whose directories no longer exist.
        let before = reg.plugins.len();
        reg.plugins.retain(|p| p.dir.join("plugin.json").is_file());
        if reg.plugins.len() != before {
            changed = true;
        }

        // Refresh existing records and discover unmanaged directories.
        if let Ok(entries) = fs::read_dir(&self.plugins_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
                let Ok(manifest) = PluginManifest::from_dir(&path) else {
                    continue;
                };
                if let Some(existing) = reg.plugins.iter_mut().find(|p| p.id == manifest.id) {
                    if existing.name != manifest.name
                        || existing.version != manifest.version
                        || existing.dir != path
                    {
                        existing.name = manifest.name;
                        existing.version = manifest.version;
                        existing.dir = path;
                        changed = true;
                    }
                } else {
                    reg.plugins.push(InstalledPluginMeta {
                        id: manifest.id,
                        name: manifest.name,
                        version: manifest.version,
                        enabled: true,
                        granted: true,
                        source: InstallSource::Discovered,
                        dir: path,
                        installed_at: now_unix_secs(),
                    });
                    changed = true;
                }
            }
        }

        reg.plugins.sort_by(|a, b| a.id.cmp(&b.id));
        if changed {
            self.save_registry(&reg)?;
        }
        Ok(reg.plugins)
    }

    fn create_staging_dir(&self, tag: &str) -> Result<PathBuf, InstallerError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = self
            .plugins_dir
            .join(format!(".staging-{tag}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|source| InstallerError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(dir)
    }

    fn build_pending_install(
        &self,
        manifest: &PluginManifest,
        staging_dir: PathBuf,
        plugin_root: PathBuf,
        source: InstallSource,
    ) -> Result<PendingPluginInstall, InstallerError> {
        if !is_valid_plugin_id(&manifest.id) {
            self.cleanup_staging_dir(&staging_dir);
            return Err(InstallerError::InvalidPluginId(manifest.id.clone()));
        }
        let report = crate::compat::analyze_plugin_dir(&plugin_root)?;
        let compat = report.to_source_compat(true);
        let locales = crate::manifest::PluginLocales::load_from_dir(&plugin_root);
        let info = manifest.to_source_info(&locales, "zh-CN");
        let is_update = self
            .plugins_dir
            .join(&manifest.id)
            .join("plugin.json")
            .is_file();
        Ok(PendingPluginInstall {
            id: manifest.id.clone(),
            name: info.name,
            version: manifest.version.clone(),
            description: manifest.description.clone(),
            author: manifest.author.clone(),
            source,
            staging_dir,
            plugin_root,
            network_hosts: compat.network_hosts,
            process_commands: compat.process_commands,
            is_update,
        })
    }

    fn cleanup_staging_dir(&self, staging_dir: &Path) {
        if staging_dir.starts_with(&self.plugins_dir)
            && staging_dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".staging-"))
        {
            let _ = fs::remove_dir_all(staging_dir);
        }
    }

    fn finalize_install(
        &self,
        manifest: &PluginManifest,
        staged_plugin_root: &Path,
        source: InstallSource,
    ) -> Result<InstalledPluginMeta, InstallerError> {
        self.finalize_install_with_grant(manifest, staged_plugin_root, source, None)
    }

    fn finalize_install_with_grant(
        &self,
        manifest: &PluginManifest,
        staged_plugin_root: &Path,
        source: InstallSource,
        explicit_granted: Option<bool>,
    ) -> Result<InstalledPluginMeta, InstallerError> {
        if !is_valid_plugin_id(&manifest.id) {
            return Err(InstallerError::InvalidPluginId(manifest.id.clone()));
        }
        let dest_dir = self.plugins_dir.join(&manifest.id);

        // Enforce `.origin.json` ownership guard (matching `mygo-music`'s `Pending.Install`):
        // a plugin installed from one GitHub repo cannot be silently overwritten by a different repo
        // or a local folder, and vice-versa.
        let (incoming_repo, incoming_version) = match &source {
            InstallSource::GitHubRepo {
                owner,
                repo,
                git_ref,
            } => (
                format!("{owner}/{repo}"),
                if git_ref.is_empty() {
                    manifest.version.clone()
                } else {
                    git_ref.clone()
                },
            ),
            _ => (String::new(), manifest.version.clone()),
        };

        if dest_dir.exists() {
            let existing_repo = read_plugin_origin(&dest_dir)
                .map(|o| o.repo)
                .unwrap_or_default();
            if existing_repo != incoming_repo {
                return Err(InstallerError::OriginConflict {
                    plugin_id: manifest.id.clone(),
                    existing_repo,
                    incoming_repo,
                });
            }
        }

        let temp_dest = self.create_staging_dir(&format!("install-{}", manifest.id))?;
        if let Err(err) = copy_dir_recursive(staged_plugin_root, &temp_dest) {
            let _ = fs::remove_dir_all(&temp_dest);
            return Err(err);
        }

        if !incoming_repo.is_empty() {
            let origin = PluginOrigin {
                repo: incoming_repo,
                version: incoming_version,
            };
            let origin_bytes = serde_json::to_vec(&origin)?;
            let origin_path = temp_dest.join(ORIGIN_FILE);
            if let Err(source_err) = fs::write(&origin_path, &origin_bytes) {
                let _ = fs::remove_dir_all(&temp_dest);
                return Err(InstallerError::Io {
                    path: origin_path,
                    source: source_err,
                });
            }
        }

        // Replace existing plugin directory safely.
        if dest_dir.exists() {
            fs::remove_dir_all(&dest_dir).map_err(|source| InstallerError::Io {
                path: dest_dir.clone(),
                source,
            })?;
        }
        fs::rename(&temp_dest, &dest_dir).map_err(|source| InstallerError::Io {
            path: dest_dir.clone(),
            source,
        })?;

        let mut reg = self.load_registry()?;
        let (enabled, prev_granted) = reg
            .plugins
            .iter()
            .find(|p| p.id == manifest.id)
            .map(|p| (p.enabled, p.granted))
            .unwrap_or((true, true));
        let granted = explicit_granted.unwrap_or(prev_granted);
        reg.plugins.retain(|p| p.id != manifest.id);

        let meta = InstalledPluginMeta {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            enabled,
            granted,
            source,
            dir: dest_dir,
            installed_at: now_unix_secs(),
        };
        reg.plugins.push(meta.clone());
        reg.plugins.sort_by(|a, b| a.id.cmp(&b.id));
        self.save_registry(&reg)?;
        Ok(meta)
    }

    fn load_registry(&self) -> Result<PluginRegistry, InstallerError> {
        if !self.registry_path.is_file() {
            return Ok(PluginRegistry::default());
        }
        let content =
            fs::read_to_string(&self.registry_path).map_err(|source| InstallerError::Io {
                path: self.registry_path.clone(),
                source,
            })?;
        Ok(serde_json::from_str(&content)?)
    }

    fn save_registry(&self, reg: &PluginRegistry) -> Result<(), InstallerError> {
        let json = serde_json::to_string_pretty(reg)?;
        atomic_write(&self.registry_path, json.as_bytes()).map_err(|source| InstallerError::Io {
            path: self.registry_path.clone(),
            source,
        })
    }
}

/// Atomically writes `data` to `path` via a sibling temporary file + rename.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_name = format!(
        ".{}.{}.{nonce}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, data)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_newer_version(current: &str, candidate: &str) -> bool {
    let parse_parts = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .take(4)
            .filter_map(|p| p.parse::<u64>().ok())
            .collect()
    };
    let cur = parse_parts(current);
    let cand = parse_parts(candidate);
    if !cur.is_empty() && !cand.is_empty() {
        let len = cur.len().max(cand.len());
        for i in 0..len {
            let a = *cur.get(i).unwrap_or(&0);
            let b = *cand.get(i).unwrap_or(&0);
            if b > a {
                return true;
            }
            if b < a {
                return false;
            }
        }
        return false;
    }
    candidate.trim() != current.trim()
}

/// Finds `plugin.json` either directly in `dir` or inside a single top-level wrapper folder.
fn locate_plugin_root(dir: &Path) -> Result<PathBuf, InstallerError> {
    if dir.join("plugin.json").is_file() {
        return Ok(dir.to_path_buf());
    }
    let entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|source| InstallerError::Io {
            path: dir.to_path_buf(),
            source,
        })?
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.') && n != "__MACOSX")
        })
        .collect();

    if entries.len() == 1 && entries[0].is_dir() && entries[0].join("plugin.json").is_file() {
        return Ok(entries[0].clone());
    }
    Err(InstallerError::Manifest(ManifestError::Validation(
        format!("plugin.json not found in {}", dir.display()),
    )))
}

/// Extracts ZIP bytes into `dest_dir` with strict zip-slip, symlink, and zip-bomb checks.
pub fn extract_zip_safely(bytes: &[u8], dest_dir: &Path) -> Result<(), InstallerError> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|e| InstallerError::Zip(e.to_string()))?;

    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(InstallerError::ArchiveLimit(format!(
            "archive contains {} entries (max {})",
            archive.len(),
            MAX_ZIP_ENTRIES
        )));
    }

    let mut total_written: u64 = 0;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| InstallerError::Zip(e.to_string()))?;
        let raw_name = entry
            .name()
            .map_err(|e| InstallerError::Zip(e.to_string()))?
            .into_owned();

        if entry.is_symlink() {
            return Err(InstallerError::SymlinkNotAllowed(raw_name));
        }

        // Strict path traversal / zip-slip verification:
        // 1. Reject raw backslashes or NUL bytes.
        if raw_name.contains('\0') {
            return Err(InstallerError::ZipSlip(raw_name));
        }
        // 2. Reject any explicit `..`, absolute, or drive-prefixed component.
        let normalized = raw_name.replace('\\', "/");
        if normalized.starts_with('/') {
            return Err(InstallerError::ZipSlip(raw_name));
        }
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| InstallerError::ZipSlip(raw_name.clone()))?;
        for comp in rel.components() {
            if !matches!(comp, Component::Normal(_)) {
                return Err(InstallerError::ZipSlip(raw_name));
            }
        }
        // Also double-check against the raw slash-normalized segments.
        for seg in normalized.split('/') {
            if seg == ".." || seg.contains(':') {
                return Err(InstallerError::ZipSlip(raw_name));
            }
        }

        let out_path = dest_dir.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|source| InstallerError::Io {
                path: out_path.clone(),
                source,
            })?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|source| InstallerError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut buf = Vec::new();
        let remaining = MAX_ZIP_UNCOMPRESSED_BYTES.saturating_sub(total_written);
        let mut limited = (&mut entry).take(remaining.saturating_add(1));
        limited
            .read_to_end(&mut buf)
            .map_err(|source| InstallerError::Io {
                path: out_path.clone(),
                source,
            })?;
        total_written = total_written.saturating_add(buf.len() as u64);
        if total_written > MAX_ZIP_UNCOMPRESSED_BYTES {
            return Err(InstallerError::ArchiveLimit(format!(
                "uncompressed archive size exceeds {} bytes",
                MAX_ZIP_UNCOMPRESSED_BYTES
            )));
        }

        fs::write(&out_path, &buf).map_err(|source| InstallerError::Io {
            path: out_path.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = if entry.unix_mode().is_some_and(|m| m & 0o111 != 0) {
                0o755
            } else {
                0o644
            };
            let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(perm));
        }
    }

    Ok(())
}

/// Reads `.origin.json` from an installed plugin directory if present.
pub fn read_plugin_origin(plugin_dir: &Path) -> Option<PluginOrigin> {
    let raw = fs::read_to_string(plugin_dir.join(ORIGIN_FILE)).ok()?;
    let origin: PluginOrigin = serde_json::from_str(&raw).ok()?;
    if origin.repo.is_empty() {
        None
    } else {
        Some(origin)
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), InstallerError> {
    fs::create_dir_all(dst).map_err(|source| InstallerError::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    let entries = fs::read_dir(src).map_err(|source| InstallerError::Io {
        path: src.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| InstallerError::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| InstallerError::Io {
            path: entry.path(),
            source,
        })?;
        let target = dst.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(InstallerError::SymlinkNotAllowed(
                entry.path().display().to_string(),
            ));
        } else if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).map_err(|source| InstallerError::Io {
                path: target.clone(),
                source,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = entry.metadata() {
                    let perm = if meta.permissions().mode() & 0o111 != 0 {
                        0o755
                    } else {
                        0o644
                    };
                    let _ = fs::set_permissions(&target, fs::Permissions::from_mode(perm));
                }
            }
        }
    }
    Ok(())
}

fn build_installer_http_client(timeout: Duration) -> Result<reqwest::Client, InstallerError> {
    reqwest::Client::builder()
        .user_agent("mygo-music")
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| InstallerError::Http(e.to_string()))
}

static GH_TOKEN_CACHE: OnceLock<Option<String>> = OnceLock::new();
static GH_TOKEN_REFUSED: AtomicBool = AtomicBool::new(false);

fn github_api_token() -> Option<String> {
    if GH_TOKEN_REFUSED.load(Ordering::Relaxed) {
        return None;
    }
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    GH_TOKEN_CACHE.get_or_init(find_gh_cli_token).clone()
}

fn find_gh_cli_token() -> Option<String> {
    #[cfg(any(target_env = "ohos", target_os = "ios", target_os = "android"))]
    {
        None
    }
    #[cfg(not(any(target_env = "ohos", target_os = "ios", target_os = "android")))]
    {
        use std::process::{Command, Stdio};
        let gh_path = crate::mobile_compat::resolve_real_binary("gh", "gh")?;
        let mut child = Command::new(gh_path)
            .args(["auth", "token", "--hostname", "github.com"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return None;
                    }
                    let mut out = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = stdout.read_to_string(&mut out);
                    }
                    let trimmed = out.trim();
                    return (!trimmed.is_empty()).then(|| trimmed.to_owned());
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => return None,
            }
        }
    }
}

async fn github_send_with_retry(
    client: &reqwest::Client,
    url: &str,
    accept: &str,
    timeout: Duration,
) -> Result<reqwest::Response, reqwest::Error> {
    let is_api = url.starts_with("https://api.github.com/");
    let mut token = if is_api { github_api_token() } else { None };
    loop {
        let mut req = client.get(url).timeout(timeout);
        if !accept.is_empty() {
            req = req.header("Accept", accept);
        }
        if let Some(ref tok) = token {
            req = req.bearer_auth(tok);
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && token.is_some() {
            log::warn!(
                target: "nsh_music_plugin",
                "GitHub refused auth token (HTTP 401); dropping token and retrying anonymously"
            );
            GH_TOKEN_REFUSED.store(true, Ordering::Relaxed);
            token = None;
            continue;
        }
        return Ok(resp);
    }
}

enum ReleaseLookup {
    Found(String, String),
    NotFound,
    Error,
}

async fn fetch_latest_release_archive(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
) -> ReleaseLookup {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let Ok(resp) = github_send_with_retry(
        client,
        &url,
        "application/vnd.github+json",
        Duration::from_secs(12),
    )
    .await
    else {
        return ReleaseLookup::Error;
    };
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return ReleaseLookup::NotFound;
    }
    if !resp.status().is_success() {
        return ReleaseLookup::Error;
    }
    let Ok(val) = resp.json::<serde_json::Value>().await else {
        return ReleaseLookup::Error;
    };
    let Some(tag) = val
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return ReleaseLookup::Error;
    };

    if let Some(assets) = val.get("assets").and_then(|v| v.as_array()) {
        let has_token = github_api_token().is_some();
        let zip_assets: Vec<(&str, &str)> = assets
            .iter()
            .filter_map(|a| {
                let name = a.get("name").and_then(|v| v.as_str())?;
                let dl = if has_token {
                    a.get("url")
                        .and_then(|v| v.as_str())
                        .or_else(|| a.get("browser_download_url").and_then(|v| v.as_str()))?
                } else {
                    a.get("browser_download_url").and_then(|v| v.as_str())?
                };
                name.to_ascii_lowercase()
                    .ends_with(".zip")
                    .then_some((name, dl))
            })
            .collect();
        if let Some((_, dl)) = pick_platform_zip_asset(&zip_assets) {
            return ReleaseLookup::Found(tag, dl.to_owned());
        }
    }

    if let Some(zipball) = val
        .get("zipball_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return ReleaseLookup::Found(tag, zipball.to_owned());
    }

    ReleaseLookup::Found(
        tag.clone(),
        format!("https://codeload.github.com/{owner}/{repo}/zip/refs/tags/{tag}"),
    )
}

async fn fetch_head_commit_archive(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
) -> Option<(String, String)> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/HEAD");
    let resp = github_send_with_retry(
        client,
        &url,
        "application/vnd.github+json",
        Duration::from_secs(12),
    )
    .await
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let val = resp.json::<serde_json::Value>().await.ok()?;
    let sha = val
        .get("sha")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let short_sha = sha[..sha.len().min(12)].to_owned();
    let zipball = format!("https://api.github.com/repos/{owner}/{repo}/zipball/{sha}");
    Some((short_sha, zipball))
}

fn pick_platform_zip_asset<'a>(assets: &[(&'a str, &'a str)]) -> Option<(&'a str, &'a str)> {
    if assets.is_empty() {
        return None;
    }
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let alt_os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        os
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        std::env::consts::ARCH
    };

    assets
        .iter()
        .copied()
        .find(|(name, _)| {
            let l = name.to_ascii_lowercase();
            (l.contains(os) || l.contains(alt_os)) && l.contains(arch)
        })
        .or_else(|| {
            assets.iter().copied().find(|(name, _)| {
                let l = name.to_ascii_lowercase();
                l.contains(os) || l.contains(alt_os)
            })
        })
        .or_else(|| {
            assets.iter().copied().find(|(name, _)| {
                let l = name.to_ascii_lowercase();
                !l.contains("windows")
                    && !l.contains("linux")
                    && !l.contains("darwin")
                    && !l.contains("macos")
            })
        })
}

async fn fetch_default_branch(client: &reqwest::Client, owner: &str, repo: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}");
    let resp = github_send_with_retry(
        client,
        &url,
        "application/vnd.github+json",
        Duration::from_secs(12),
    )
    .await
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let val = resp.json::<serde_json::Value>().await.ok()?;
    val.get("default_branch")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn run_future_blocking<F, T>(fut: F) -> Result<T, InstallerError>
where
    F: std::future::Future<Output = Result<T, InstallerError>> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| InstallerError::Http(e.to_string()))?;
            rt.block_on(fut)
        })
        .join()
        .map_err(|_| InstallerError::Http("download thread panicked".to_owned()))?
    } else {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| InstallerError::Http(e.to_string()))?;
        rt.block_on(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn temp_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nsh-music-installer-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn parses_github_specs() {
        let a = GitHubRepoSpec::parse("alice/my-plugin").expect("shorthand");
        assert_eq!(a.owner, "alice");
        assert_eq!(a.repo, "my-plugin");
        assert_eq!(a.git_ref, None);

        let b = GitHubRepoSpec::parse("alice/my-plugin@v1.0.0").expect("with ref");
        assert_eq!(b.git_ref.as_deref(), Some("v1.0.0"));

        let c = GitHubRepoSpec::parse("https://github.com/alice/my-plugin/tree/dev")
            .expect("github tree url");
        assert_eq!(c.owner, "alice");
        assert_eq!(c.repo, "my-plugin");
        assert_eq!(c.git_ref.as_deref(), Some("dev"));

        let d = GitHubRepoSpec::parse("https://github.com/GentleM4dman/mygo-music-bilibili")
            .expect("full github url");
        assert_eq!(d.owner, "GentleM4dman");
        assert_eq!(d.repo, "mygo-music-bilibili");
        assert_eq!(d.git_ref, None);

        let e = GitHubRepoSpec::parse("github.com/GentleM4dman/mygo-music-bilibili/")
            .expect("bare github host url");
        assert_eq!(e.owner, "GentleM4dman");
        assert_eq!(e.repo, "mygo-music-bilibili");

        let f = GitHubRepoSpec::parse("git@github.com:GentleM4dman/mygo-music-bilibili.git")
            .expect("ssh github url");
        assert_eq!(f.owner, "GentleM4dman");
        assert_eq!(f.repo, "mygo-music-bilibili");

        let g = GitHubRepoSpec::parse("https://github.com/alice/my-plugin/releases/tag/v0.5.0")
            .expect("release tag url");
        assert_eq!(g.git_ref.as_deref(), Some("v0.5.0"));

        assert!(GitHubRepoSpec::parse("https://evil.example.com/a/b").is_err());
        assert!(GitHubRepoSpec::parse("../bad/repo").is_err());
    }

    #[test]
    fn installs_zip_and_blocks_zip_slip() {
        let root = temp_test_dir("zip");
        let installer = PluginInstaller::new(&root).expect("installer");

        // 1. Valid ZIP with top-level wrapper folder (GitHub archive layout).
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            zw.start_file("demo-plugin-main/plugin.json", opts).unwrap();
            zw.write_all(
                br#"{"id":"zip-demo","name":"Zip Demo","version":"1.0.0","main":"index.js"}"#,
            )
            .unwrap();
            zw.start_file("demo-plugin-main/index.js", opts).unwrap();
            zw.write_all(b"export async function search() { return { tracks: [] }; }")
                .unwrap();
            zw.finish().unwrap();
        }
        let meta = installer
            .install_from_zip_bytes(
                buf.get_ref(),
                InstallSource::ZipArchive {
                    path: "demo.zip".to_owned(),
                },
            )
            .expect("valid zip installs");
        assert_eq!(meta.id, "zip-demo");
        assert!(meta.enabled);
        assert!(meta.dir.join("index.js").is_file());

        // Toggle enabled & uninstall.
        let disabled = installer.set_enabled("zip-demo", false).expect("disable");
        assert!(!disabled.enabled);
        installer.uninstall("zip-demo").expect("uninstall");
        assert!(installer.list_installed().unwrap().is_empty());

        // 2. Malicious ZIP with zip-slip `../escape.txt` entry MUST fail.
        let mut evil_buf = Cursor::new(Vec::new());
        {
            let mut zw = ZipWriter::new(&mut evil_buf);
            let opts = SimpleFileOptions::default();
            zw.start_file("../escape.txt", opts).unwrap();
            zw.write_all(b"pwned").unwrap();
            zw.finish().unwrap();
        }
        let err = installer
            .install_from_zip_bytes(
                evil_buf.get_ref(),
                InstallSource::ZipArchive {
                    path: "evil.zip".to_owned(),
                },
            )
            .expect_err("zip slip must be rejected");
        assert!(matches!(err, InstallerError::ZipSlip(_)), "got {err:?}");
        assert!(!root.join("escape.txt").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn enforces_origin_json_guard_and_preserves_unix_executable_bits() {
        let root = temp_test_dir("origin-exec");
        let installer = PluginInstaller::new(&root).expect("installer");

        let make_zip = |version: &str| -> Vec<u8> {
            let mut buf = Cursor::new(Vec::new());
            {
                let mut zw = ZipWriter::new(&mut buf);
                let regular = SimpleFileOptions::default().unix_permissions(0o644);
                let exec = SimpleFileOptions::default().unix_permissions(0o755);
                zw.start_file("pkg/plugin.json", regular).unwrap();
                zw.write_all(
                    format!(r#"{{"id":"origin-demo","name":"Origin Demo","version":"{version}","main":"index.js"}}"#)
                        .as_bytes(),
                )
                .unwrap();
                zw.start_file("pkg/index.js", regular).unwrap();
                zw.write_all(b"export async function suggest() { return []; }")
                    .unwrap();
                zw.start_file("pkg/bin/helper", exec).unwrap();
                zw.write_all(b"#!/bin/sh\necho ok\n").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };

        // 1. Install from GitHub repo `alice/origin-demo` writes `.origin.json` and preserves 0o755.
        let zip_v1 = make_zip("1.0.0");
        let meta = installer
            .install_from_zip_bytes(
                &zip_v1,
                InstallSource::GitHubRepo {
                    owner: "alice".to_owned(),
                    repo: "origin-demo".to_owned(),
                    git_ref: "v1.0.0".to_owned(),
                },
            )
            .expect("install from alice/origin-demo");
        let origin = read_plugin_origin(&meta.dir).expect(".origin.json must be written");
        assert_eq!(origin.repo, "alice/origin-demo");
        assert_eq!(origin.version, "v1.0.0");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let helper_mode = fs::metadata(meta.dir.join("bin/helper"))
                .expect("helper exists")
                .permissions()
                .mode();
            assert_ne!(helper_mode & 0o111, 0, "executable bit must be preserved");
        }

        // 2. Updating from the SAME repo succeeds.
        let zip_v2 = make_zip("1.1.0");
        let meta2 = installer
            .install_from_zip_bytes(
                &zip_v2,
                InstallSource::GitHubRepo {
                    owner: "alice".to_owned(),
                    repo: "origin-demo".to_owned(),
                    git_ref: "v1.1.0".to_owned(),
                },
            )
            .expect("update from same repo succeeds");
        assert_eq!(read_plugin_origin(&meta2.dir).unwrap().version, "v1.1.0");

        // 3. Overwriting from a DIFFERENT GitHub repo is blocked!
        let conflict_gh = installer
            .install_from_zip_bytes(
                &zip_v2,
                InstallSource::GitHubRepo {
                    owner: "bob".to_owned(),
                    repo: "origin-demo".to_owned(),
                    git_ref: "v2.0.0".to_owned(),
                },
            )
            .expect_err("cross-repo overwrite must fail");
        assert!(
            matches!(conflict_gh, InstallerError::OriginConflict { .. }),
            "expected OriginConflict, got {conflict_gh:?}"
        );

        // 4. Overwriting a GitHub plugin from a local zip is also blocked!
        let conflict_local = installer
            .install_from_zip_bytes(
                &zip_v2,
                InstallSource::ZipArchive {
                    path: "local.zip".to_owned(),
                },
            )
            .expect_err("local overwrite of github plugin must fail");
        assert!(
            matches!(conflict_local, InstallerError::OriginConflict { .. }),
            "expected OriginConflict, got {conflict_local:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}

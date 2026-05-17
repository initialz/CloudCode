//! Self-update for the hub.
//!
//! Same shape as `crates/agent/src/update.rs`, just for the hub
//! binary: download tarball + sha256 manifest into
//! `~/.local/state/cloudcode/hub/versions/<vX.Y.Z>/`, verify the
//! binary hash, then atomically flip the
//! `~/.local/state/cloudcode/hub/current` symlink. The supervisor
//! sees the in-process `exit(0)` and relaunches via that freshly
//! pointed-at binary.
//!
//! Triggered by an admin-UI button rather than a remote message
//! (the hub IS the remote-end for agents); see
//! `admin/api.rs::hub_update`.
//!
//! We never replace the running binary in place. The install dir
//! lives per-version forever (until manually GC'd) so previous
//! releases stay around for rollback.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const VERSION_RE: &str = r"^v\d+\.\d+\.\d+$";
const HUB_BINARY_NAME: &str = "cloudcode-hub";

pub struct UpdateRequest {
    pub target_version: String,
    pub download_url: String,
    pub sha256_url: String,
}

pub async fn perform_update(req: UpdateRequest) -> Result<(), String> {
    tracing::info!(
        target = %req.target_version,
        "hub self-update starting"
    );
    if !is_valid_version(&req.target_version) {
        return Err(format!(
            "invalid target_version {:?}; must match {}",
            req.target_version, VERSION_RE
        ));
    }

    let state = state_dir().ok_or_else(|| "could not determine state dir".to_string())?;
    let hub_dir = state.join("hub");
    let versions_dir = hub_dir.join("versions");
    let install_dir = versions_dir.join(&req.target_version);
    std::fs::create_dir_all(&install_dir)
        .map_err(|e| format!("create {}: {}", install_dir.display(), e))?;

    let tarball_path = install_dir.join("download.tar.gz");
    let sha256_path = install_dir.join("sha256.txt");
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent(format!("cloudcode-hub/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("build http client: {}", e))?;

    download_to_file(&client, &req.download_url, &tarball_path).await?;
    download_to_file(&client, &req.sha256_url, &sha256_path).await?;

    extract_tar_gz(&tarball_path, &install_dir)
        .map_err(|e| format!("extract tarball: {}", e))?;

    let extracted_binary = find_hub_binary(&install_dir).ok_or_else(|| {
        format!(
            "no `{}` found in extracted tarball under {}",
            HUB_BINARY_NAME,
            install_dir.display()
        )
    })?;
    let actual_hash = sha256_file(&extracted_binary)
        .map_err(|e| format!("hash {}: {}", extracted_binary.display(), e))?;

    let expected_hash = read_expected_hash(&sha256_path, HUB_BINARY_NAME).ok_or_else(|| {
        format!(
            "could not find {} entry in {}",
            HUB_BINARY_NAME,
            sha256_path.display()
        )
    })?;
    if !actual_hash.eq_ignore_ascii_case(&expected_hash) {
        let _ = std::fs::remove_dir_all(&install_dir);
        return Err(format!(
            "sha256 mismatch for {}: expected {}, got {}",
            HUB_BINARY_NAME, expected_hash, actual_hash
        ));
    }

    set_executable(&extracted_binary).map_err(|e| format!("chmod: {}", e))?;

    let final_binary = install_dir.join(HUB_BINARY_NAME);
    if final_binary != extracted_binary {
        std::fs::copy(&extracted_binary, &final_binary)
            .map_err(|e| format!("copy binary to install root: {}", e))?;
        set_executable(&final_binary).map_err(|e| format!("chmod final: {}", e))?;
    }

    update_current_symlink(&hub_dir, &final_binary)
        .map_err(|e| format!("update symlink: {}", e))?;

    tracing::info!(
        version = %req.target_version,
        binary = %final_binary.display(),
        "hub self-update installed"
    );

    Ok(())
}

fn is_valid_version(v: &str) -> bool {
    let Some(rest) = v.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {}: {}", url, e))?;
    if !resp.status().is_success() {
        return Err(format!("GET {}: HTTP {}", url, resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body for {}: {}", url, e))?;
    let dest = dest.to_path_buf();
    let bytes_vec = bytes.to_vec();
    tokio::task::spawn_blocking(move || std::fs::write(&dest, &bytes_vec))
        .await
        .map_err(|e| format!("join write: {}", e))?
        .map_err(|e| format!("write file: {}", e))
}

fn extract_tar_gz(tarball: &Path, dest: &Path) -> std::io::Result<()> {
    let f = std::fs::File::open(tarball)?;
    let dec = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(dec);
    ar.set_preserve_permissions(false);
    ar.unpack(dest)
}

fn find_hub_binary(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file()
                && path.file_name().and_then(|n| n.to_str()) == Some(HUB_BINARY_NAME)
            {
                return Some(path);
            }
        }
    }
    None
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_expected_hash(manifest_path: &Path, name: &str) -> Option<String> {
    let body = std::fs::read_to_string(manifest_path).ok()?;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let hash = it.next()?.trim();
        let rest = it.next()?.trim_start_matches('*').trim();
        let path = Path::new(rest);
        let file_name = path.file_name().and_then(|n| n.to_str());
        if file_name == Some(name) {
            return Some(hash.to_string());
        }
    }
    None
}

fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm)
}

fn update_current_symlink(hub_dir: &Path, new_target: &Path) -> std::io::Result<()> {
    let current = hub_dir.join("current");
    let previous = hub_dir.join("previous");
    let tmp = hub_dir.join("current.tmp");

    if let Ok(old_target) = std::fs::read_link(&current) {
        let _ = std::fs::remove_file(&previous);
        std::os::unix::fs::symlink(&old_target, &previous)?;
    }

    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(new_target, &tmp)?;
    std::fs::rename(&tmp, &current)?;
    Ok(())
}

pub fn state_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLOUDCODE_STATE_DIR") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(base.join("cloudcode"))
}

/// Compile-time host triple, emitted by build.rs into `CLOUDCODE_TARGET`.
/// Used by the admin-UI update flow to pick the matching release asset.
pub fn target_triple() -> &'static str {
    env!("CLOUDCODE_TARGET")
}

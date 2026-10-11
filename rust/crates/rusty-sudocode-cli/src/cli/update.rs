//! Release discovery, startup notices and explicit CLI self-update.
//!
//! Mirrors the release layout that `install.sh` publishes and installs against,
//! so a binary installed either way can update itself:
//!   - latest tag  → `GET https://api.github.com/repos/<repo>/releases/latest`
//!   - archive     → `…/releases/download/<tag>/scode-<target>.tar.gz`,
//!     overridable with `SCODE_MIRROR` for faster CN downloads
//!   - checksums   → the same published manifest on the selected release source
//!
//! Flow: resolve the target tag, download the archive + checksums, verify
//! SHA-256, extract `scode-<target>/scode`, atomically swap it over the running
//! executable (rename on the same filesystem — safe while the old image stays
//! mapped), then re-exec the new binary so the caller lands on the new version.

use sha2::{Digest, Sha256};
use std::error::Error;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::{FsBackend, StdFsBackend};

const REPO: &str = "sudoprivacy/sudocode";
/// The version compiled into *this* binary (no leading `v`).
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const BIN_NAME: &str = "scode";
const CHECKSUM_FILE: &str = "SHA256SUMS.txt";
const USER_AGENT: &str = concat!("scode-update/", env!("CARGO_PKG_VERSION"));
const COS_LATEST: &str =
    "https://sudowork-release-1309794936.cos.ap-beijing.myqcloud.com/sudocode/release/latest";
const CHECK_INTERVAL_SECS: u64 = 20 * 60 * 60;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Release {
    tag: String,
    download_base: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ReleaseCache {
    checked_at: u64,
    source: String,
    release: Release,
}

/// Read the cached notice and refresh metadata off the terminal's startup path.
/// The next launch displays the refreshed version, as in Codex's startup check.
pub(crate) fn startup_notice() -> Option<String> {
    let is_enabled = match std::env::var("SCODE_CHECK_UPDATES").as_deref() {
        Ok("0" | "false" | "off") => false,
        Ok("1" | "true" | "on") => true,
        _ => !cfg!(debug_assertions),
    };
    if !is_enabled {
        return None;
    }
    let source = configured_mirror().unwrap_or_default();
    let path = runtime::config::default_config_home()
        .join("cache")
        .join("release.json");
    let cached = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<ReleaseCache>(&raw).ok())
        .filter(|cache| cache.source == source && is_stable_tag(&cache.release.tag));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs());
    let is_fresh = cached.as_ref().is_some_and(|cache| {
        cache.checked_at <= now && now - cache.checked_at < CHECK_INTERVAL_SECS
    });
    if !is_fresh {
        let _ = std::thread::Builder::new()
            .name("scode-release-check".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let Ok(release) = runtime.block_on(resolve_release(None)) else {
                    return;
                };
                let cache = ReleaseCache {
                    checked_at: now,
                    source,
                    release,
                };
                let Ok(raw) = serde_json::to_vec(&cache) else {
                    return;
                };
                if let Some(parent) = path.parent() {
                    if std::fs::create_dir_all(parent).is_ok() {
                        let _ = StdFsBackend.write_atomic(&path.to_string_lossy(), &raw);
                    }
                }
            });
    }
    let cache = cached.filter(|cache| is_newer(&cache.release.tag, &format!("v{CURRENT}")))?;
    #[cfg(windows)]
    let action = format!(
        "Download {}/scode-windows-{}.zip",
        cache.release.download_base,
        if std::env::consts::ARCH == "aarch64" {
            "arm64"
        } else {
            "x64"
        }
    );
    #[cfg(not(windows))]
    let action = "Run scode update";
    Some(format!(
        "Update available: v{CURRENT} → {}. {action}.",
        cache.release.tag
    ))
}

type R<T> = Result<T, Box<dyn Error>>;

/// Entry point for the `update` subcommand.
///
/// * `version` — pin a release tag (`v0.1.27` or `0.1.27`); `None` = latest.
/// * `check`   — only report current vs. latest, download nothing.
/// * `yes`     — skip the interactive confirmation.
pub(crate) fn run(version: Option<&str>, check: bool, yes: bool) -> R<()> {
    let current_tag = format!("v{CURRENT}");

    // Networked steps run on a short-lived runtime — `main` is synchronous and
    // spins up runtimes on demand, so there is no ambient one to nest inside.
    let rt = tokio::runtime::Runtime::new()?;

    let pinned = version.is_some();
    let release = rt.block_on(resolve_release(version))?;
    let latest = &release.tag;

    println!("scode {current_tag} → {latest}");

    let up_to_date = !is_newer(latest, &current_tag);
    if check {
        println!(
            "{}",
            if up_to_date {
                "up to date"
            } else {
                "update available"
            }
        );
        return Ok(());
    }
    // An explicit --version may pin an older tag on purpose; only short-circuit
    // for the default (latest) path.
    if up_to_date && !pinned {
        println!("already up to date");
        return Ok(());
    }
    let target = detect_target()?;
    if !yes && !confirm(&format!("update {current_tag} → {latest}?"))? {
        println!("aborted");
        return Ok(());
    }

    let archive = format!("scode-{target}.tar.gz");
    let archive_url = format!("{}/{archive}", release.download_base);
    let checksum_url = format!("{}/{CHECKSUM_FILE}", release.download_base);

    println!("downloading {archive}");
    let (bytes, sums) = rt.block_on(async {
        let bytes = fetch_bytes(&archive_url).await?;
        let sums = fetch_text(&checksum_url).await?;
        Ok::<_, Box<dyn Error>>((bytes, sums))
    })?;

    verify_checksum(&bytes, &sums, &archive)?;
    println!("checksum verified");

    let new_binary = extract_binary(&bytes, &target)?;

    let exe = current_exe()?;
    install_over(&exe, &new_binary)?;
    println!("installed {latest} → {}", exe.display());

    restart(&exe)
}

// ---------------------------------------------------------------------------
// target / version
// ---------------------------------------------------------------------------

/// The release asset infix for this platform, matching `install.sh`'s
/// `detect_target` (`scode-<target>.tar.gz`).
fn detect_target() -> R<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x64",
        other => return Err(format!("unsupported architecture: {other}").into()),
    };
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        other => return Err(format!("unsupported OS for self-update: {other}").into()),
    };
    Ok(format!("{os}-{arch}"))
}

/// Ensure a `vX.Y.Z` tag shape (accepts a bare `X.Y.Z`).
fn normalize_tag(v: &str) -> String {
    if v.starts_with('v') {
        v.to_string()
    } else {
        format!("v{v}")
    }
}

/// `true` if `candidate` is a strictly newer version than `current`.
/// Compares the numeric `X.Y.Z` triples; falls back to a string inequality if
/// either tag doesn't parse (so an unusual tag still allows updating).
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_semver(candidate), parse_semver(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => candidate != current,
    }
}

fn parse_semver(tag: &str) -> Option<(u64, u64, u64)> {
    let t = tag.strip_prefix('v').unwrap_or(tag);
    let mut it = t.split('.').map(|part| {
        if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
            part.parse::<u64>().ok()
        } else {
            None
        }
    });
    let version = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(version)
}

// ---------------------------------------------------------------------------
// network
// ---------------------------------------------------------------------------

fn client() -> R<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent(USER_AGENT).build()?)
}

fn configured_mirror() -> Option<String> {
    std::env::var("SCODE_MIRROR")
        .ok()
        .map(|base| base.trim().trim_end_matches('/').to_string())
        .filter(|base| !base.is_empty())
}

fn is_stable_tag(tag: &str) -> bool {
    tag.starts_with('v') && parse_semver(tag).is_some()
}

/// Resolve one immutable release. Explicit mirrors keep all metadata and
/// content requests on that source; the default falls back from GitHub to COS.
async fn resolve_release(version: Option<&str>) -> R<Release> {
    if let Some(base) = configured_mirror() {
        return mirror_release(&base, version).await;
    }
    let tag = match version {
        Some(version) => normalize_tag(version),
        None => match resolve_latest_tag().await {
            Ok(tag) if is_stable_tag(&tag) => tag,
            _ => return mirror_release(COS_LATEST, None).await,
        },
    };
    Ok(Release {
        download_base: format!("https://github.com/{REPO}/releases/download/{tag}"),
        tag,
    })
}

async fn mirror_release(base: &str, version: Option<&str>) -> R<Release> {
    let (tag, download_base) = if let Some(root) = base.strip_suffix("/latest") {
        let tag = match version {
            Some(version) => normalize_tag(version),
            None => metadata_text(&format!("{base}/version.txt"))
                .await?
                .trim()
                .to_string(),
        };
        let download_base = format!("{root}/{tag}");
        (tag, download_base)
    } else {
        let tag = base.rsplit('/').next().unwrap_or_default().to_string();
        if version.is_some_and(|version| normalize_tag(version) != tag) {
            return Err("mirror directory differs from requested version".into());
        }
        (tag, base.to_string())
    };
    if !is_stable_tag(&tag) {
        return Err("mirror must name a stable vX.Y.Z release".into());
    }
    Ok(Release { tag, download_base })
}

async fn metadata_text(url: &str) -> R<String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut raw = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if raw.len() + chunk.len() > 64 * 1024 {
            return Err("release metadata exceeds 64 KiB".into());
        }
        raw.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8(raw)?)
}

/// Resolve the latest release tag from the GitHub API.
async fn resolve_latest_tag() -> R<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let value: serde_json::Value = serde_json::from_str(&metadata_text(&url).await?)?;
    value
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "could not read tag_name from GitHub API response".into())
}

async fn fetch_bytes(url: &str) -> R<Vec<u8>> {
    let resp = client()?.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

async fn fetch_text(url: &str) -> R<String> {
    let resp = client()?.get(url).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

// ---------------------------------------------------------------------------
// verify / extract / install
// ---------------------------------------------------------------------------

/// Check the archive's SHA-256 against its line in `SHA256SUMS.txt`
/// (`<hex>␠␠scode-<target>.tar.gz`).
fn verify_checksum(bytes: &[u8], sums: &str, archive: &str) -> R<()> {
    let expected = sums
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(archive))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| format!("no checksum entry for {archive} in {CHECKSUM_FILE}"))?;
    let actual = hex(&Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(
            format!("checksum mismatch for {archive}: expected {expected}, got {actual}").into(),
        );
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Pull the `scode` binary out of the `.tar.gz` (entry `scode-<target>/scode`).
fn extract_binary(archive: &[u8], target: &str) -> R<Vec<u8>> {
    let want = format!("scode-{target}/{BIN_NAME}");
    let gz = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        // Match the documented path, but also accept a bare `scode` file name
        // in case the archive layout is flattened.
        let is_match = path == Path::new(&want) || path.file_name().is_some_and(|n| n == BIN_NAME);
        if is_match {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            if !buf.is_empty() {
                return Ok(buf);
            }
        }
    }
    Err(format!("`{BIN_NAME}` not found inside the release archive").into())
}

/// Resolve the real path of the running executable (following symlinks so we
/// replace the actual file, not a link to it).
fn current_exe() -> R<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// Atomically replace `exe` with `new_binary`: write to a sibling temp file on
/// the same filesystem, mark it executable, then `rename` over the target. The
/// running process keeps its already-mapped image, so this is safe live.
fn install_over(exe: &Path, new_binary: &[u8]) -> R<()> {
    let dir = exe
        .parent()
        .ok_or("cannot determine the install directory")?;
    let tmp = dir.join(format!(".{BIN_NAME}.update.{}", std::process::id()));

    let write_tmp = || -> R<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(new_binary)?;
        f.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    };

    if let Err(e) = write_tmp().and_then(|()| Ok(std::fs::rename(&tmp, exe)?)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "failed to install into {} ({e}). If it is a system path, re-run with sudo \
             or reinstall via install.sh.",
            dir.display()
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// restart / prompt
// ---------------------------------------------------------------------------

/// Restart into the freshly installed binary. On Unix this `exec`s in place
/// (the process image becomes the new `scode`), so it never returns on success;
/// we run `--version` so the caller sees the new version and a clean exit.
fn restart(exe: &Path) -> R<()> {
    println!("restarting…");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe).arg("--version").exec();
        // `exec` only returns on failure.
        Err(format!("failed to restart {}: {err}", exe.display()).into())
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(exe).arg("--version").status()?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

/// Ask the user to confirm on the terminal. A non-interactive/closed stdin is
/// treated as "no" — use `--yes` to update unattended.
fn confirm(prompt: &str) -> R<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(false);
    }
    let a = line.trim().to_ascii_lowercase();
    Ok(a == "y" || a == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn version_comparison() {
        assert!(is_newer("v0.1.27", "v0.1.26"));
        assert!(is_newer("v0.2.0", "v0.1.99"));
        assert!(is_newer("v1.0.0", "v0.9.9"));
        assert!(!is_newer("v0.1.26", "v0.1.26"));
        assert!(!is_newer("v0.1.25", "v0.1.26"));
        // Un-parseable tags fall back to string inequality.
        assert!(is_newer("nightly", "v0.1.26"));
        assert!(!is_newer("same", "same"));
    }

    #[test]
    fn tag_normalization() {
        assert_eq!(normalize_tag("0.1.27"), "v0.1.27");
        assert_eq!(normalize_tag("v0.1.27"), "v0.1.27");
    }

    #[test]
    fn checksum_accepts_match_and_rejects_mismatch() {
        let bytes = b"the release archive bytes";
        let archive = "scode-macos-arm64.tar.gz";
        let sum = hex(&Sha256::digest(bytes));
        let sums = format!("{sum}  {archive}\ndead…  scode-linux-x64.tar.gz\n");
        assert!(verify_checksum(bytes, &sums, archive).is_ok());
        assert!(verify_checksum(b"tampered", &sums, archive).is_err());
        // Missing entry is an error, not a silent pass.
        assert!(verify_checksum(bytes, &sums, "scode-linux-arm64.tar.gz").is_err());
    }

    #[test]
    fn extracts_scode_from_archive() {
        let target = "macos-arm64";
        let payload = b"#!/fake/scode binary payload";
        let archive = make_archive(&format!("scode-{target}/scode"), payload);
        let got = extract_binary(&archive, target).unwrap();
        assert_eq!(got, payload);
    }

    /// Build a `.tar.gz` holding one file at `inner_path`.
    fn make_archive(inner_path: &str, content: &[u8]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, inner_path, content)
                .unwrap();
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }
}

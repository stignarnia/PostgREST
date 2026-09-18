use crate::health;
use reqwest::Client;
use serde::Deserialize;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub static ENABLED: AtomicBool = AtomicBool::new(true);

const LATEST_RELEASE: &str = "https://api.github.com/repos/stignarnia/PostgREST/releases/latest";
// A crash loop must not also become a download loop.
const FIRST_CHECK: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

// Release asset for this platform and the magic bytes its file must start with.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: Option<(&str, &[u8])> = Some(("PostgREST-linux-amd64", b"\x7fELF"));
#[cfg(all(windows, target_arch = "x86_64"))]
const ASSET: Option<(&str, &[u8])> = Some(("PostgREST-windows-amd64.exe", b"MZ"));
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64")
)))]
const ASSET: Option<(&str, &[u8])> = None;

pub type Version = (u64, u64, u64);

pub enum Outcome {
    UpToDate(Version),
    Installed(Version),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
}

pub fn parse_version(s: &str) -> Option<Version> {
    let mut parts = s.strip_prefix('v').unwrap_or(s).split('.');
    let v = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(v)
}

pub fn current() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).expect("package version is x.y.z")
}

pub fn format_version((a, b, c): Version) -> String {
    format!("v{a}.{b}.{c}")
}

/// Installs the latest GitHub release over the running executable if it is
/// strictly newer. Errors are fixed labels: nothing fetched reaches a message.
pub async fn check_and_install() -> Result<Outcome, &'static str> {
    let (asset_name, magic) = ASSET.ok_or("no release asset for this platform")?;
    let client = Client::builder()
        .user_agent(concat!("PostgREST/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|_| "http client setup failed")?;

    let release: Release = client
        .get(LATEST_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|_| "release check failed")?
        .json()
        .await
        .map_err(|_| "release check failed")?;

    let latest = parse_version(&release.tag_name).ok_or("latest release tag is not vX.Y.Z")?;
    if latest <= current() {
        return Ok(Outcome::UpToDate(latest));
    }

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or("latest release has no asset for this platform")?;
    let bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|_| "download failed")?
        .bytes()
        .await
        .map_err(|_| "download failed")?;
    if bytes.len() as u64 != asset.size {
        return Err("download size mismatch");
    }
    if !bytes.starts_with(magic) {
        return Err("download is not an executable for this platform");
    }

    tokio::task::spawn_blocking(move || install(&bytes))
        .await
        .map_err(|_| "replace failed")??;
    Ok(Outcome::Installed(latest))
}

fn install(bytes: &[u8]) -> Result<(), &'static str> {
    let exe = std::env::current_exe().map_err(|_| "cannot locate own executable")?;
    let dir = exe.parent().ok_or("cannot locate own executable")?;
    // Same directory as the executable, so the swap is a rename on one volume.
    let tmp = dir.join(".PostgREST-update");
    std::fs::write(&tmp, bytes).map_err(|_| "cannot write update next to executable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|_| "cannot mark update executable")?;
    }
    let result = self_replace::self_replace(&tmp).map_err(|_| "replace failed");
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Background loop inside the server. After installing, it exits with status 1
/// so the service manager restarts the process on the new binary.
pub async fn run() {
    let enabled = ENABLED.load(Ordering::Relaxed);
    health::update_updater(|u| u.enabled = enabled);
    if !enabled {
        return;
    }

    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        let result = check_and_install().await;
        health::update_updater(|u| {
            u.last_check = Some(health::now());
            match &result {
                Ok(Outcome::UpToDate(v) | Outcome::Installed(v)) => {
                    u.latest_release = Some(format_version(*v));
                    u.last_error = None;
                }
                Err(e) => u.last_error = Some(e),
            }
        });
        match result {
            Ok(Outcome::Installed(v)) => {
                eprintln!("updater: installed {}, restarting", format_version(v));
                std::process::exit(1);
            }
            Ok(Outcome::UpToDate(_)) => {}
            Err(e) => {
                eprintln!("updater: {e}");
                health::record("updater", e, None, None);
            }
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

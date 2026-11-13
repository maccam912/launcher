//! Updating the launcher itself from its own GitHub releases.
//!
//! Each release carries one bare executable per platform (see
//! `.github/workflows/release.yml`). The running binary is swapped in place,
//! so it works the same inside a macOS `.app`, from `~/.local/bin`, or on
//! Windows where a running `.exe` cannot simply be overwritten.

use crate::{
    catalog::Platform,
    github::{Client, Release},
    install::Store,
};
use std::{fs, process::Command};

pub const REPO: &str = "maccam912/launcher";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub fn asset_name(platform: Platform) -> Option<String> {
    match (platform.os, platform.arch) {
        ("macos", _) => Some("launcher-macos-universal".into()),
        ("linux", arch @ ("x86_64" | "aarch64")) => Some(format!("launcher-linux-{arch}")),
        ("windows", "x86_64") => Some("launcher-windows-x86_64.exe".into()),
        _ => None,
    }
}

/// True when release tag `tag` (`v1.2.3`) is a newer version than `current`.
pub fn is_newer(tag: &str, current: &str) -> bool {
    let parse = |text: &str| semver::Version::parse(text.trim_start_matches('v')).ok();
    match (parse(tag), parse(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The release to move to, if there is one for this platform.
pub fn available(client: &Client, platform: Platform) -> Result<Option<Release>, String> {
    let Some(release) = client.latest_release(REPO)? else {
        return Ok(None);
    };
    let has_build = asset_name(platform)
        .is_some_and(|name| release.assets.iter().any(|asset| asset.name == name));
    Ok((is_newer(&release.tag_name, CURRENT) && has_build).then_some(release))
}

/// Download `release` and replace the running executable with it. The new
/// version takes effect on the next start; see [`restart`].
pub fn apply(client: &Client, store: &Store, platform: Platform, release: &Release) -> Result<(), String> {
    let name = asset_name(platform).ok_or("No launcher builds exist for this platform")?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| format!("Release {} has no {name}", release.tag_name))?;
    // Game packages may predate GitHub's digests; the launcher's own builds never do.
    if asset.digest.as_deref().is_none_or(|digest| !digest.starts_with("sha256:")) {
        return Err(format!("Release {} has no checksum for {name}", release.tag_name));
    }
    let download = store.root.join("launcher-update.part");
    let result = client.download(asset, &download, |_, _| {}).and_then(|()| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&download, fs::Permissions::from_mode(0o755))
                .map_err(|error| error.to_string())?;
        }
        self_replace::self_replace(&download)
            .map_err(|error| format!("Cannot replace the launcher program: {error}"))
    });
    let _ = fs::remove_file(&download);
    result
}

/// Start the (freshly replaced) executable and end this process.
pub fn restart() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).args(std::env::args_os().skip(1)).spawn();
    }
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_and_names_assets() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("v0.2.0-rc.1", "0.2.0"));
        let name = |os, arch| asset_name(Platform { os, arch });
        assert_eq!(name("macos", "aarch64").unwrap(), "launcher-macos-universal");
        assert_eq!(name("linux", "x86_64").unwrap(), "launcher-linux-x86_64");
        assert_eq!(name("windows", "x86_64").unwrap(), "launcher-windows-x86_64.exe");
        assert_eq!(name("freebsd", "x86_64"), None);
    }
}

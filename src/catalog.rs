//! The list of things the launcher can install, and how a release asset is
//! matched to the machine it is running on.

use crate::github::Asset;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Catalog compiled into the binary; used until a newer one is fetched.
pub const EMBEDDED: &str = include_str!("../catalog.json");

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Catalog {
    #[serde(default)]
    pub version: u64,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    /// `owner/name` of the GitHub repository whose releases are installed.
    pub repo: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    /// Optional asset-name globs keyed by `os-arch` or `os`. Without one the
    /// asset is picked by looking for platform words in its name.
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
    /// Optional executable path inside the package, keyed like `assets`.
    #[serde(default)]
    pub exe: BTreeMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_kind() -> String {
    "game".into()
}

impl Catalog {
    pub fn parse(text: &str) -> Result<Catalog, String> {
        let catalog: Catalog =
            serde_json::from_str(text).map_err(|error| format!("Invalid catalog: {error}"))?;
        for entry in &catalog.entries {
            let safe_id = !entry.id.is_empty()
                && entry
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            let mut repo = entry.repo.split('/');
            let safe_repo = matches!(
                (repo.next(), repo.next(), repo.next()),
                (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
            ) && entry
                .repo
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
            if !safe_id || !safe_repo {
                return Err(format!("Invalid catalog entry {:?}", entry.id));
            }
        }
        Ok(catalog)
    }

    pub fn embedded() -> Catalog {
        Catalog::parse(EMBEDDED).expect("embedded catalog is valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    pub fn current() -> Platform {
        Platform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    fn keys(&self) -> [String; 2] {
        [format!("{}-{}", self.os, self.arch), self.os.to_string()]
    }

    /// Catalog override for this platform, most specific key first.
    pub fn lookup<'a>(&self, map: &'a BTreeMap<String, String>) -> Option<&'a str> {
        self.keys()
            .iter()
            .find_map(|key| map.get(key).map(String::as_str))
    }
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

const NOT_A_PACKAGE: &[&str] = &[
    "launcher", "server", "sha256", "manifest", "checksum", ".apk", ".aab", ".txt", ".json",
    ".sig", ".asc", ".pck", ".deb", ".rpm", ".dmg", ".msi",
];

fn os_words(os: &str) -> &'static [&'static str] {
    match os {
        "macos" => &["macos", "darwin", "osx", "-mac", "_mac"],
        "linux" => &["linux"],
        "windows" => &["windows", "win64", "win32", ".exe"],
        _ => &[],
    }
}

fn arch_words(arch: &str) -> &'static [&'static str] {
    match arch {
        "aarch64" => &["aarch64", "arm64"],
        "x86_64" => &["x86_64", "amd64", "x64"],
        _ => &[],
    }
}

/// Pick the release asset to install on `platform`, or `None` when the release
/// has no build for it.
pub fn pick_asset<'a>(entry: &Entry, assets: &'a [Asset], platform: Platform) -> Option<&'a Asset> {
    if let Some(pattern) = platform.lookup(&entry.assets) {
        return assets.iter().find(|asset| glob_match(pattern, &asset.name));
    }
    let has = |name: &str, words: &[&str]| words.iter().any(|word| name.contains(word));
    assets
        .iter()
        .filter_map(|asset| {
            let name = asset.name.to_ascii_lowercase();
            if has(&name, NOT_A_PACKAGE) || !has(&name, os_words(platform.os)) {
                return None;
            }
            let other = if platform.arch == "aarch64" { "x86_64" } else { "aarch64" };
            let score = if has(&name, arch_words(platform.arch)) {
                3
            } else if name.contains("universal") || !has(&name, arch_words(other)) {
                2
            } else if platform.os == "macos" && platform.arch == "aarch64" {
                1 // Intel build, runs under Rosetta
            } else {
                return None;
            };
            Some((score, asset))
        })
        // max_by_key keeps the last maximum; reverse so the first listed wins ties.
        .rev()
        .max_by_key(|(score, _)| *score)
        .map(|(_, asset)| asset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets(names: &[&str]) -> Vec<Asset> {
        names
            .iter()
            .map(|name| Asset {
                name: name.to_string(),
                size: 1,
                browser_download_url: String::new(),
                digest: None,
            })
            .collect()
    }

    fn entry() -> Entry {
        Catalog::embedded().entries.remove(0)
    }

    const MAC: Platform = Platform { os: "macos", arch: "aarch64" };
    const LINUX: Platform = Platform { os: "linux", arch: "x86_64" };
    const WINDOWS: Platform = Platform { os: "windows", arch: "x86_64" };

    fn pick(names: &[&str], platform: Platform) -> Option<String> {
        pick_asset(&entry(), &assets(names), platform).map(|asset| asset.name.clone())
    }

    #[test]
    fn embedded_catalog_parses_and_rejects_unsafe_ids() {
        assert!(Catalog::embedded().entries.len() >= 4);
        let bad = r#"{"entries":[{"id":"../x","name":"x","repo":"a/b"}]}"#;
        assert!(Catalog::parse(bad).is_err());
        let bad = r#"{"entries":[{"id":"x","name":"x","repo":"a/b/c"}]}"#;
        assert!(Catalog::parse(bad).is_err());
    }

    #[test]
    fn picks_real_release_layouts() {
        let simulateds = [
            "simulateds-android.apk",
            "simulateds-linux.zip",
            "simulateds-linux.zip.sha256",
            "simulateds-macos.zip",
            "simulateds-server.zip",
            "simulateds-windows.zip",
        ];
        assert_eq!(pick(&simulateds, MAC).unwrap(), "simulateds-macos.zip");
        assert_eq!(pick(&simulateds, LINUX).unwrap(), "simulateds-linux.zip");
        assert_eq!(pick(&simulateds, WINDOWS).unwrap(), "simulateds-windows.zip");

        let paint = [
            "paint-n-seek-launcher-linux-x86_64.tar.gz",
            "paint-n-seek-launcher-macos-universal.zip",
            "paint-n-seek-launcher-windows-x86_64.exe",
            "paint-n-seek-linux-x86_64.zip",
            "paint-n-seek-macos-universal.zip",
            "paint-n-seek-windows-x86_64.zip",
            "SHA256SUMS.txt",
        ];
        assert_eq!(pick(&paint, MAC).unwrap(), "paint-n-seek-macos-universal.zip");
        assert_eq!(pick(&paint, LINUX).unwrap(), "paint-n-seek-linux-x86_64.zip");

        let rubblekin = [
            "client-manifest.json",
            "rubblekin-client-aarch64-apple-darwin.zip",
            "rubblekin-client-x86_64-apple-darwin.zip",
            "rubblekin-client-x86_64-pc-windows-msvc.zip",
            "rubblekin-client-x86_64-unknown-linux-gnu.zip",
            "rubblekin-launcher-aarch64-apple-darwin.zip",
        ];
        assert_eq!(
            pick(&rubblekin, MAC).unwrap(),
            "rubblekin-client-aarch64-apple-darwin.zip"
        );
        assert_eq!(
            pick(&rubblekin, Platform { os: "macos", arch: "x86_64" }).unwrap(),
            "rubblekin-client-x86_64-apple-darwin.zip"
        );
        assert_eq!(
            pick(&rubblekin, WINDOWS).unwrap(),
            "rubblekin-client-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn missing_platform_and_wrong_arch_are_not_installed() {
        let raw = ["app-macos-arm64", "app-macos-x86_64", "app-windows-amd64.exe"];
        assert_eq!(pick(&raw, MAC).unwrap(), "app-macos-arm64");
        assert_eq!(pick(&raw, LINUX), None);
        assert_eq!(pick(&["game-linux-arm64.zip"], LINUX), None);
        // Apple silicon falls back to an Intel build.
        assert_eq!(pick(&["game-macos-x86_64.zip"], MAC).unwrap(), "game-macos-x86_64.zip");
    }

    #[test]
    fn catalog_glob_overrides_the_guess() {
        let mut entry = entry();
        entry.assets.insert("macos".into(), "Game-*-mac.zip".into());
        let list = assets(&["Game-7-mac.zip", "game-macos.zip"]);
        assert_eq!(pick_asset(&entry, &list, MAC).unwrap().name, "Game-7-mac.zip");
        assert!(glob_match("a*b*c", "a-b-b-c"));
        assert!(!glob_match("a*b", "a-b-c"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }
}

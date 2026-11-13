//! Minimal GitHub releases client: newest release per repository (with an
//! ETag cache so repeat checks do not use up the anonymous rate limit) and
//! verified asset downloads.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    /// ISO 8601 UTC, so plain string comparison orders it.
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
    /// `sha256:<hex>` as computed by GitHub at upload time.
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    etag: String,
    body: String,
}

pub struct Client {
    agent: ureq::Agent,
    cache_dir: PathBuf,
    token: Option<String>,
}

/// The most recently published stable release, or the most recent
/// pre-release when a repository has never published a stable one. GitHub's
/// list order is not reliable, so this goes by publication time.
pub fn choose_release(releases: Vec<Release>) -> Option<Release> {
    releases
        .into_iter()
        .filter(|release| !release.draft)
        .max_by(|a, b| (!a.prerelease, &a.published_at).cmp(&(!b.prerelease, &b.published_at)))
}

impl Client {
    pub fn new(cache_dir: PathBuf) -> Client {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .user_agent(concat!("game-launcher/", env!("CARGO_PKG_VERSION")))
                .timeout_connect(Some(Duration::from_secs(15)))
                .timeout_recv_response(Some(Duration::from_secs(30)))
                .build(),
        );
        Client {
            agent,
            cache_dir,
            token: std::env::var("GITHUB_TOKEN").ok().filter(|token| !token.is_empty()),
        }
    }

    /// Newest installable release of `repo`; `None` when it has none. This is
    /// the release GitHub marks "Latest", or for a repository that only
    /// publishes pre-releases, the most recent of those.
    pub fn latest_release(&self, repo: &str) -> Result<Option<Release>, String> {
        let key = repo.replace('/', "__");
        let parse_error = |error| format!("Unexpected response from GitHub for {repo}: {error}");
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        if let Some(body) = self.get_cached(&url, &format!("latest-{key}"))? {
            return serde_json::from_str(&body).map(Some).map_err(parse_error);
        }
        let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
        let Some(body) = self.get_cached(&url, &format!("releases-{key}"))? else {
            return Ok(None);
        };
        serde_json::from_str(&body).map(choose_release).map_err(parse_error)
    }

    /// GET with `If-None-Match`. Falls back to the cached body when GitHub is
    /// unreachable or rate limiting. `None` means the URL does not exist.
    pub fn get_cached(&self, url: &str, key: &str) -> Result<Option<String>, String> {
        let cache_path = self.cache_dir.join(format!("{key}.json"));
        let cached: Option<Cached> = fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let mut request = self
            .agent
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if url.starts_with("https://api.github.com/")
            && let Some(token) = &self.token
        {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        if let Some(cached) = &cached {
            request = request.header("If-None-Match", &cached.etag);
        }
        let mut response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(error) => {
                return match cached {
                    Some(cached) => Ok(Some(cached.body)),
                    None => Err(describe(&error)),
                };
            }
        };
        if response.status().as_u16() == 304
            && let Some(cached) = cached
        {
            return Ok(Some(cached.body));
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| describe(&error))?;
        if let Some(etag) = etag {
            let _ = fs::create_dir_all(&self.cache_dir);
            let cached = Cached { etag, body: body.clone() };
            let _ = fs::write(&cache_path, serde_json::to_vec(&cached).unwrap_or_default());
        }
        Ok(Some(body))
    }

    /// Stream `asset` to `dest`, checking its size and (when GitHub published
    /// one) its SHA-256 before reporting success.
    pub fn download(
        &self,
        asset: &Asset,
        dest: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<(), String> {
        let mut response = self
            .agent
            .get(&asset.browser_download_url)
            .config()
            .timeout_global(Some(Duration::from_secs(2 * 60 * 60)))
            .build()
            .call()
            .map_err(|error| format!("Cannot download {}: {}", asset.name, describe(&error)))?;
        let mut reader = response.body_mut().as_reader();
        let mut file = File::create(dest)
            .map_err(|error| format!("Cannot write {}: {error}", dest.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("Download of {} was interrupted: {error}", asset.name))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(|error| format!("Cannot write {}: {error}", dest.display()))?;
            done += read as u64;
            progress(done, asset.size);
        }
        file.flush().map_err(|error| error.to_string())?;
        if done != asset.size {
            return Err(format!(
                "Download of {} was incomplete ({done} of {} bytes)",
                asset.name, asset.size
            ));
        }
        if let Some(expected) = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")) {
            let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(format!("{} failed its checksum; not installing it", asset.name));
            }
        }
        Ok(())
    }
}

fn describe(error: &ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(403 | 429) => {
            "GitHub is rate limiting this network; try again in a while".into()
        }
        ureq::Error::StatusCode(code) => format!("GitHub answered with HTTP {code}"),
        other => format!("Cannot reach GitHub ({other})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool, prerelease: bool, published: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft,
            prerelease,
            published_at: Some(format!("2026-10-{published}T00:00:00Z")),
            assets: vec![],
        }
    }

    #[test]
    fn prefers_latest_published_stable_then_latest_prerelease() {
        let pick = |list: Vec<Release>| choose_release(list).map(|release| release.tag_name);
        assert_eq!(pick(vec![]), None);
        assert_eq!(pick(vec![release("draft", true, false, "09")]), None);
        assert_eq!(
            pick(vec![
                release("draft", true, false, "09"),
                release("3-dev", false, true, "08"),
                release("2", false, false, "07"),
            ]),
            Some("2".into())
        );
        assert_eq!(
            pick(vec![release("2-dev", false, true, "02"), release("3-dev", false, true, "03")]),
            Some("3-dev".into())
        );
        // Listed out of order, as GitHub does for releases created before they are published.
        assert_eq!(
            pick(vec![
                release("b", false, false, "02"),
                release("c", false, false, "03"),
                release("a", false, false, "01"),
            ]),
            Some("c".into())
        );
    }
}

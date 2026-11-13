//! On-disk layout, package extraction and launching.
//!
//! ```text
//! <root>/state.json
//! <root>/cache/                      GitHub ETag cache, last catalog
//! <root>/games/<id>/versions/<tag>/  one extracted release
//! <root>/games/<id>/data/            stable working directory (survives updates)
//! <root>/games/<id>/game.log
//! ```

use crate::{
    catalog::{Entry, Platform, pick_asset},
    github::{Client, Release},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Installed {
    pub tag: String,
    /// Executable path relative to the version directory.
    pub exe: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub auto_update_games: bool,
    pub installed: BTreeMap<String, Installed>,
}

impl Default for State {
    fn default() -> State {
        State { auto_update_games: true, installed: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Downloading { done: u64, total: u64 },
    Installing,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn open(root: Option<PathBuf>) -> Result<Store, String> {
        let root = match root {
            Some(root) => root,
            None => dirs::data_dir()
                .ok_or("Cannot find a per-user data directory; pass --data-dir")?
                .join("game-launcher"),
        };
        fs::create_dir_all(root.join("games"))
            .map_err(|error| format!("Cannot create {}: {error}", root.display()))?;
        Ok(Store { root })
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    fn game_dir(&self, id: &str) -> PathBuf {
        self.root.join("games").join(id)
    }

    fn version_dir(&self, id: &str, tag: &str) -> PathBuf {
        self.game_dir(id).join("versions").join(safe_name(tag))
    }

    pub fn load_state(&self) -> State {
        fs::read(self.root.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, state: &State) -> Result<(), String> {
        let path = self.root.join("state.json");
        let temp = self.root.join("state.json.tmp");
        let bytes = serde_json::to_vec_pretty(state).map_err(|error| error.to_string())?;
        fs::write(&temp, bytes)
            .and_then(|()| fs::rename(&temp, &path))
            .map_err(|error| format!("Cannot save {}: {error}", path.display()))
    }

    /// True when the recorded install is still present on disk.
    pub fn is_intact(&self, id: &str, installed: &Installed) -> bool {
        self.version_dir(id, &installed.tag).join(&installed.exe).is_file()
    }

    /// Download and unpack `release`. The previous version stays in place
    /// until the new one is fully extracted, then it is removed.
    pub fn install(
        &self,
        client: &Client,
        entry: &Entry,
        release: &Release,
        platform: Platform,
        mut progress: impl FnMut(Progress),
    ) -> Result<Installed, String> {
        let asset = pick_asset(entry, &release.assets, platform).ok_or_else(|| {
            format!("{} {} has no build for {}-{}", entry.name, release.tag_name, platform.os, platform.arch)
        })?;
        let game_dir = self.game_dir(&entry.id);
        let versions = game_dir.join("versions");
        fs::create_dir_all(&versions).map_err(|error| error.to_string())?;
        let package = game_dir.join("download.part");
        client.download(asset, &package, |done, total| {
            progress(Progress::Downloading { done, total })
        })?;
        progress(Progress::Installing);

        let final_dir = self.version_dir(&entry.id, &release.tag_name);
        let staging = versions.join(format!("{}.partial", safe_name(&release.tag_name)));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
        let result = unpack(&package, &asset.name, &staging).and_then(|()| {
            let exe = match platform.lookup(&entry.exe) {
                Some(exe) if staging.join(exe).is_file() => exe.to_string(),
                Some(exe) => return Err(format!("{} does not contain {exe}", asset.name)),
                None => find_executable(&staging, platform.os)
                    .ok_or_else(|| format!("Cannot find a program to run inside {}", asset.name))?,
            };
            make_executable(&staging.join(&exe))?;
            Ok(exe)
        });
        let _ = fs::remove_file(&package);
        let exe = match result {
            Ok(exe) => exe,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        let _ = fs::remove_dir_all(&final_dir);
        fs::rename(&staging, &final_dir)
            .map_err(|error| format!("Cannot finish installing {}: {error}", entry.name))?;
        // Old versions (and any abandoned partial extractions) are no longer needed.
        if let Ok(dirs) = fs::read_dir(&versions) {
            for dir in dirs.flatten() {
                if dir.path() != final_dir {
                    let _ = fs::remove_dir_all(dir.path());
                }
            }
        }
        Ok(Installed { tag: release.tag_name.clone(), exe })
    }

    /// Removes the program files. The `data` directory is left alone.
    pub fn uninstall(&self, id: &str) -> Result<(), String> {
        let versions = self.game_dir(id).join("versions");
        match fs::remove_dir_all(&versions) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("Cannot remove {}: {error}", versions.display()))
            }
            _ => Ok(()),
        }
    }

    pub fn launch(&self, entry: &Entry, installed: &Installed) -> Result<Child, String> {
        let game_dir = self.game_dir(&entry.id);
        let exe = self.version_dir(&entry.id, &installed.tag).join(&installed.exe);
        let cwd = game_dir.join("data");
        fs::create_dir_all(&cwd).map_err(|error| error.to_string())?;
        let log = File::create(game_dir.join("game.log")).map_err(|error| error.to_string())?;
        let error_log = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = Command::new(&exe);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        command
            .args(&entry.args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log))
            .spawn()
            .map_err(|error| format!("Cannot start {}: {error}", entry.name))
    }
}

fn safe_name(tag: &str) -> String {
    let name: String = tag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    // Never "." / ".." / hidden.
    format!("v{name}")
}

fn unpack(package: &Path, asset_name: &str, dest: &Path) -> Result<(), String> {
    let name = asset_name.to_ascii_lowercase();
    let failed = |error: &dyn std::fmt::Display| format!("Cannot unpack {asset_name}: {error}");
    let file = File::open(package).map_err(|error| failed(&error))?;
    if name.ends_with(".zip") {
        zip::ZipArchive::new(file)
            .and_then(|mut archive| archive.extract(dest))
            .map_err(|error| failed(&error))
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(dest)
            .map_err(|error| failed(&error))
    } else {
        // A bare program published as the asset itself.
        drop(file);
        fs::copy(package, dest.join(asset_name))
            .map(|_| ())
            .map_err(|error| failed(&error))
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).map_err(|error| error.to_string())?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn is_native_binary(path: &Path, os: &str) -> bool {
    let mut magic = [0u8; 4];
    if File::open(path).and_then(|mut file| file.read_exact(&mut magic)).is_err() {
        return false;
    }
    match os {
        "linux" => magic == [0x7f, b'E', b'L', b'F'],
        "macos" => matches!(magic, [0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe]),
        _ => false,
    }
}

/// Find the program to run inside an unpacked release: the `.app` bundle on
/// macOS, the `.exe` on Windows, the ELF binary on Linux. Shallowest wins,
/// then largest. Returns a path relative to `root`.
pub fn find_executable(root: &Path, os: &str) -> Option<String> {
    // (depth, size, path); candidates are compared by depth, then by size descending.
    let mut best: Option<(usize, u64, PathBuf)> = None;
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(children) = fs::read_dir(&dir) else { continue };
        for child in children.flatten() {
            let path = child.path();
            let Ok(kind) = child.file_type() else { continue };
            let name = child.file_name().to_string_lossy().to_ascii_lowercase();
            let candidate = if kind.is_dir() {
                if os == "macos" && name.ends_with(".app") {
                    bundle_executable(&path)
                } else {
                    if depth < 4 {
                        pending.push((path, depth + 1));
                    }
                    continue;
                }
            } else if kind.is_file() {
                let wanted = match os {
                    "windows" => name.ends_with(".exe") && !name.ends_with(".console.exe"),
                    _ => {
                        !name.contains(".so") && !name.ends_with(".dylib") && is_native_binary(&path, os)
                    }
                };
                wanted.then_some(path)
            } else {
                None
            };
            let Some(candidate) = candidate else { continue };
            let size = fs::metadata(&candidate).map(|meta| meta.len()).unwrap_or(0);
            // An .app bundle beats any loose binary at the same depth.
            let size = if name.ends_with(".app") { u64::MAX } else { size };
            let better = best
                .as_ref()
                .is_none_or(|(d, s, _)| (depth, std::cmp::Reverse(size)) < (*d, std::cmp::Reverse(*s)));
            if better {
                best = Some((depth, size, candidate));
            }
        }
    }
    let (_, _, path) = best?;
    let relative = path.strip_prefix(root).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// The main binary of a macOS `.app`: the one named after the bundle if
/// present, otherwise the largest file in `Contents/MacOS`.
fn bundle_executable(app: &Path) -> Option<PathBuf> {
    let dir = app.join("Contents").join("MacOS");
    let stem = app.file_stem()?.to_string_lossy().to_ascii_lowercase();
    let mut files: Vec<(bool, u64, PathBuf)> = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|child| child.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|child| {
            let named = child.file_name().to_string_lossy().to_ascii_lowercase() == stem;
            let size = child.metadata().map(|meta| meta.len()).unwrap_or(0);
            (named, size, child.path())
        })
        .collect();
    files.sort();
    files.pop().map(|(_, _, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    const ELF: &[u8] = b"\x7fELF-and-then-some";
    const MACHO: &[u8] = b"\xcf\xfa\xed\xfe-and-then-some";

    #[test]
    fn finds_programs_in_typical_godot_and_rust_packages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("OFL.txt"), b"text");
        write(&root.join("Game.app/Contents/Info.plist"), b"plist");
        write(&root.join("Game.app/Contents/MacOS/helper"), MACHO);
        write(&root.join("Game.app/Contents/MacOS/Game"), MACHO);
        write(&root.join("Game.app/Contents/Frameworks/libx.dylib"), MACHO);
        assert_eq!(find_executable(root, "macos").unwrap(), "Game.app/Contents/MacOS/Game");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("pkg/game.pck"), b"a much larger data file than the binary");
        write(&root.join("pkg/libgodot_steam.so"), ELF);
        write(&root.join("pkg/game.x86_64"), ELF);
        write(&root.join("pkg/run.sh"), b"#!/bin/sh");
        assert_eq!(find_executable(root, "linux").unwrap(), "pkg/game.x86_64");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("game.console.exe"), b"MZ-small-wrapper-but-padded-to-be-bigger");
        write(&root.join("game.exe"), b"MZ");
        write(&root.join("tools/other.exe"), b"MZ-bigger-but-deeper-down-the-tree");
        assert_eq!(find_executable(root, "windows").unwrap(), "game.exe");

        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("readme.txt"), b"nothing to run");
        assert_eq!(find_executable(dir.path(), "linux"), None);
    }

    #[test]
    fn unpacks_zip_tarball_and_bare_binary() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("package");

        let mut writer = zip::ZipWriter::new(File::create(&package).unwrap());
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        writer.start_file("pkg/game.x86_64", options).unwrap();
        writer.write_all(ELF).unwrap();
        writer.finish().unwrap();
        let out = dir.path().join("zip");
        fs::create_dir(&out).unwrap();
        unpack(&package, "Game-linux.ZIP", &out).unwrap();
        assert_eq!(fs::read(out.join("pkg/game.x86_64")).unwrap(), ELF);

        let encoder = flate2::write::GzEncoder::new(File::create(&package).unwrap(), Default::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(ELF.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "game", ELF).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let out = dir.path().join("tar");
        fs::create_dir(&out).unwrap();
        unpack(&package, "game-linux.tar.gz", &out).unwrap();
        assert_eq!(find_executable(&out, "linux").unwrap(), "game");

        fs::write(&package, ELF).unwrap();
        let out = dir.path().join("raw");
        fs::create_dir(&out).unwrap();
        unpack(&package, "tool-linux-x86_64", &out).unwrap();
        assert_eq!(find_executable(&out, "linux").unwrap(), "tool-linux-x86_64");

        fs::write(&package, b"not a zip").unwrap();
        assert!(unpack(&package, "broken.zip", &out).is_err());
    }

    #[test]
    fn state_round_trips_and_tags_become_safe_directory_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().join("root"))).unwrap();
        assert!(store.load_state().auto_update_games);
        let mut state = State { auto_update_games: false, ..State::default() };
        let installed = Installed { tag: "release/1.0".into(), exe: "game".into() };
        state.installed.insert("game".into(), installed.clone());
        store.save_state(&state).unwrap();
        let loaded = store.load_state();
        assert!(!loaded.auto_update_games);
        assert_eq!(loaded.installed["game"], installed);

        assert_eq!(safe_name("release/1.0"), "vrelease_1.0");
        assert_eq!(safe_name(".."), "v..");
        assert!(!store.is_intact("game", &installed));
        write(&store.version_dir("game", "release/1.0").join("game"), ELF);
        assert!(store.is_intact("game", &installed));
        store.uninstall("game").unwrap();
        assert!(!store.is_intact("game", &installed));
        store.uninstall("game").unwrap();
    }
}

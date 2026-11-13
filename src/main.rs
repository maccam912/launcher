#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod catalog;
mod github;
mod gui;
mod install;
mod selfupdate;

use catalog::{Catalog, Platform, pick_asset};
use github::Client;
use install::{Progress, Store};
use std::{io::Write, path::PathBuf};

const CATALOG_URL: &str = "https://raw.githubusercontent.com/maccam912/launcher/main/catalog.json";

const HELP: &str = "\
Usage: launcher [--data-dir DIR] [COMMAND]

With no command the launcher window opens.

Commands:
  list            Show every game, its installed and latest version
  install ID      Install or update one game
  update          Update the launcher and every installed game
  play ID         Start an installed game
  uninstall ID    Remove a game's program files (its data is kept)

Set GITHUB_TOKEN to raise GitHub's rate limit.
";

/// The newest catalog we can get: the published one (or its cached copy when
/// offline), unless the one compiled into this build is newer.
fn load_catalog(client: &Client) -> Catalog {
    let embedded = Catalog::embedded();
    match client.get_cached(CATALOG_URL, "catalog") {
        Ok(Some(text)) => match Catalog::parse(&text) {
            Ok(remote) if remote.version >= embedded.version => remote,
            _ => embedded,
        },
        _ => embedded,
    }
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let mut data_dir = None;
    if args.peek().map(String::as_str) == Some("--data-dir") {
        args.next();
        data_dir = args.next().map(PathBuf::from);
    }
    let command: Vec<String> = args.collect();
    let result = Store::open(data_dir).and_then(|store| {
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        match words.as_slice() {
            [] => gui::run(store),
            ["--version" | "-V"] => {
                println!("launcher {}", selfupdate::CURRENT);
                Ok(())
            }
            ["--help" | "-h" | "help"] => {
                print!("{HELP}");
                Ok(())
            }
            _ => cli(store, &words),
        }
    });
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn cli(store: Store, words: &[&str]) -> Result<(), String> {
    let client = Client::new(store.cache_dir());
    let platform = Platform::current();
    let catalog = load_catalog(&client);
    let mut state = store.load_state();
    let find = |id: &str| {
        catalog
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| format!("No game called {id:?}; try `launcher list`"))
    };
    let install = |entry: &catalog::Entry, state: &mut install::State| -> Result<(), String> {
        let release = client
            .latest_release(&entry.repo)?
            .ok_or_else(|| format!("{} has no releases yet", entry.name))?;
        if state.installed.get(&entry.id).is_some_and(|installed| {
            installed.tag == release.tag_name && store.is_intact(&entry.id, installed)
        }) {
            println!("{} is up to date ({})", entry.name, release.tag_name);
            return Ok(());
        }
        let mut last = u64::MAX;
        let installed = store.install(&client, entry, &release, platform, |progress| {
            if let Progress::Downloading { done, total } = progress {
                let percent = done * 100 / total.max(1);
                if percent != last {
                    last = percent;
                    print!("\r{} {}: {percent}%", entry.name, release.tag_name);
                    let _ = std::io::stdout().flush();
                }
            }
        })?;
        println!("\r{} {} installed        ", entry.name, installed.tag);
        state.installed.insert(entry.id.clone(), installed);
        store.save_state(state)
    };
    match words {
        ["list"] => {
            for entry in &catalog.entries {
                let installed = state
                    .installed
                    .get(&entry.id)
                    .filter(|installed| store.is_intact(&entry.id, installed))
                    .map_or("-", |installed| installed.tag.as_str());
                let latest = match client.latest_release(&entry.repo) {
                    Ok(Some(release)) if pick_asset(entry, &release.assets, platform).is_some() => {
                        release.tag_name
                    }
                    Ok(Some(release)) => format!("{} (no build for this platform)", release.tag_name),
                    Ok(None) => "no releases".into(),
                    Err(error) => error,
                };
                println!("{:<14} {:<22} installed: {installed:<16} latest: {latest}", entry.id, entry.name);
            }
            Ok(())
        }
        ["install", id] => install(find(id)?, &mut state),
        ["update"] => {
            for entry in &catalog.entries {
                if state.installed.contains_key(&entry.id) {
                    install(entry, &mut state)?;
                }
            }
            match selfupdate::available(&client, platform)? {
                Some(release) => {
                    selfupdate::apply(&client, &store, platform, &release)?;
                    println!("Launcher updated to {}", release.tag_name);
                }
                None => println!("Launcher {} is up to date", selfupdate::CURRENT),
            }
            Ok(())
        }
        ["play", id] => {
            let entry = find(id)?;
            let installed = state
                .installed
                .get(&entry.id)
                .filter(|installed| store.is_intact(&entry.id, installed))
                .ok_or_else(|| format!("{} is not installed; run `launcher install {id}`", entry.name))?;
            store.launch(entry, installed).map(|_| ())
        }
        ["uninstall", id] => {
            let entry = find(id)?;
            store.uninstall(&entry.id)?;
            state.installed.remove(&entry.id);
            store.save_state(&state)
        }
        _ => Err(HELP.into()),
    }
}

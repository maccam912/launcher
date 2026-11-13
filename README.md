# Launcher

A small desktop launcher that installs, updates and starts games published as
GitHub releases, and keeps itself up to date the same way. macOS, Linux and
Windows; written in Rust with egui.

## Install

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/maccam912/launcher/main/install.sh | sh
```

Windows: download `launcher-windows-x86_64.exe` from the latest release and run it.

## Adding a game

Add an entry to [`catalog.json`](catalog.json), bump its `version`, and push to
`main`. Running launchers fetch the catalog from the repository, so no launcher
release is needed.

```json
{ "id": "my-game", "name": "My Game", "repo": "owner/repo", "description": "…" }
```

The launcher installs the release GitHub marks "Latest" (or the newest
pre-release if the repository has only those). It picks the asset whose name
mentions this platform (`macos`/`darwin`, `linux`, `windows`, plus `arm64`,
`x86_64`, `universal`…), ignoring launchers, servers, checksums and Android
packages. `.zip`, `.tar.gz` and bare executables work. If the guess is wrong,
pin it:

```json
"assets": { "macos": "MyGame-*-mac.zip", "linux-x86_64": "MyGame-*-linux.zip" },
"exe":    { "linux": "MyGame/bin/mygame" },
"args":   ["--fullscreen"]
```

Downloads are checked against the SHA-256 GitHub records for each asset. Games
run with a per-game working directory that survives updates.

## Releasing the launcher

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The release workflow builds every platform. Running launchers download the new
executable, swap it in, and offer a restart.

## Command line

`launcher list | install ID | update | play ID | uninstall ID`, with an optional
leading `--data-dir DIR`. Set `GITHUB_TOKEN` to raise GitHub's rate limit.

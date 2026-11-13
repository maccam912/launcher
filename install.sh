#!/bin/sh
# Install the launcher for the current user (macOS and Linux):
#   curl -fsSL https://raw.githubusercontent.com/maccam912/launcher/main/install.sh | sh
# After this it keeps itself up to date.
set -eu
base="https://github.com/maccam912/launcher/releases/latest/download"
case "$(uname -s)" in
  Darwin)
    dest="$HOME/Applications"
    tmp="$(mktemp -d)"
    curl -fL --progress-bar -o "$tmp/Launcher.zip" "$base/Launcher-macos.zip"
    mkdir -p "$dest" && rm -rf "$dest/Launcher.app"
    ditto -x -k "$tmp/Launcher.zip" "$dest" && rm -rf "$tmp"
    echo "Installed $dest/Launcher.app"
    open "$dest/Launcher.app"
    ;;
  Linux)
    case "$(uname -m)" in
      x86_64) arch=x86_64 ;;
      aarch64 | arm64) arch=aarch64 ;;
      *) echo "No launcher build for $(uname -m)" >&2; exit 1 ;;
    esac
    bin="$HOME/.local/bin"
    apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    mkdir -p "$bin" "$apps"
    curl -fL --progress-bar -o "$bin/launcher.new" "$base/launcher-linux-$arch"
    chmod +x "$bin/launcher.new" && mv "$bin/launcher.new" "$bin/launcher"
    printf '[Desktop Entry]\nType=Application\nName=Launcher\nExec=%s\nCategories=Game;\nTerminal=false\n' \
      "$bin/launcher" > "$apps/launcher.desktop"
    echo "Installed $bin/launcher (also in your applications menu)"
    ;;
  *) echo "Unsupported system; download a build from $base" >&2; exit 1 ;;
esac

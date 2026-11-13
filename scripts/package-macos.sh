#!/bin/sh
# Build dist/launcher-macos-universal (the self-update asset) and
# dist/Launcher-macos.zip (Launcher.app, for first installs).
set -eu
cd "$(dirname "$0")/.."
rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
cargo build --release --locked --target aarch64-apple-darwin --target x86_64-apple-darwin
rm -rf dist/Launcher.app && mkdir -p dist/Launcher.app/Contents/MacOS
cp packaging/macos/Info.plist dist/Launcher.app/Contents/Info.plist
lipo -create -output dist/Launcher.app/Contents/MacOS/launcher \
  target/aarch64-apple-darwin/release/launcher target/x86_64-apple-darwin/release/launcher
codesign --force --sign - dist/Launcher.app
# The self-update asset is the bundle's executable, signature included, so an
# updated app still verifies (Info.plist never changes between versions).
cp dist/Launcher.app/Contents/MacOS/launcher dist/launcher-macos-universal
(cd dist && rm -f Launcher-macos.zip && ditto -c -k --keepParent Launcher.app Launcher-macos.zip)

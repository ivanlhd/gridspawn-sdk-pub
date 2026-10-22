#!/usr/bin/env bash
# Builds the release library for this platform (Linux) and assembles the addon.
#
#   ./package.sh [path/to/gridspawn_godot.dll]   # the Windows build, made on Windows
#
# Output: dist/gridspawn-godot-<version>/addons/gridspawn/ and a .zip of it.
set -euo pipefail
cd "$(dirname "$0")"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
cargo build --release
name="gridspawn-godot-$version"
out="dist/$name/addons/gridspawn"
rm -rf "dist/$name" "dist/$name.zip"
mkdir -p "$out/bin"
find addons/gridspawn -maxdepth 1 -type f -exec cp {} "$out/" \;
cp target/release/libgridspawn_godot.so "$out/bin/libgridspawn_godot.linux.x86_64.so"
if [ $# -ge 1 ]; then
  cp "$1" "$out/bin/gridspawn_godot.windows.x86_64.dll"
fi
python3 -c "import shutil, sys; shutil.make_archive(sys.argv[1], 'zip', sys.argv[2])" \
  "dist/$name" "dist/$name"
ls -l "$out/bin" "dist/$name.zip"

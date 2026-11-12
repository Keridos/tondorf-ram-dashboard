#!/usr/bin/env bash
# Runs the RAM dashboard with its database at ~/.ramdashboard/data.sqlite.
# Any arguments are passed to cargo, e.g. `./run.sh --release`.
set -euo pipefail

# Rocket.toml is looked up from the working directory, so run from the repo.
cd "$(dirname "$(readlink -f "$0")")"

data_dir="$HOME/.ramdashboard"
mkdir -p "$data_dir"
export ROCKET_DATABASE_PATH="$data_dir/data.sqlite"

if command -v cargo >/dev/null; then
    exec cargo run "$@"
else
    # No global toolchain (e.g. NixOS): get one from nixpkgs. gcc builds the bundled SQLite.
    exec nix-shell -p cargo rustc gcc --run "cargo run $(printf '%q ' "$@")"
fi

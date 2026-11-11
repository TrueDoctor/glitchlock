#!/usr/bin/env bash
# Runs glitchlock inside a nested sway window, so only that window gets locked.
# Extra arguments are passed to glitchlock, e.g. `--typing-ms 150 --reject-ms 800`.
# After unlocking, press Escape to close the window.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
nix build "$root" --out-link "$root/result"

config=$(mktemp)
trap 'rm -f "$config"' EXIT

image=${GLITCHLOCK_IMAGE:-/etc/nixos/modules/home/squirrel-dark.png}
command=("$root/result/bin/glitchlock" --image "$image" --pam pam-any-password --pam pam-any-fingerprint "$@")

{
    printf 'bindsym Escape exit\n'
    printf 'exec %s\n' "$(printf '%q ' "${command[@]}")"
} >"$config"

WLR_BACKENDS=wayland WLR_RENDERER=pixman nix shell nixpkgs#sway -c sway -c "$config"

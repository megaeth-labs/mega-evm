#!/usr/bin/env bash
# Install the custom-operator mutation toolchain: universalmutator + comby.
#
# Preferred path is mise (universalmutator is declared in .mise.toml). This
# script is the no-mise fallback and also installs comby, which is intentionally
# NOT mise-managed (its prebuilt links against Debian sonames — fine on Ubuntu CI
# but on Arch it must be the patched AUR build, which mise must not shadow).
set -euo pipefail

# --- universalmutator (`mutate`, `analyze_mutants`) ---------------------------
if command -v mise >/dev/null 2>&1; then
    echo "Installing universalmutator via mise (.mise.toml)..."
    mise install
elif ! command -v mutate >/dev/null 2>&1; then
    echo "mise not found; installing universalmutator into a venv..."
    VENV="${HOME}/.local/share/umutate-venv"
    python3 -m venv "$VENV"
    "$VENV/bin/pip" install --quiet universalmutator
    mkdir -p "${HOME}/.local/bin"
    for s in mutate analyze_mutants; do ln -sf "$VENV/bin/$s" "${HOME}/.local/bin/$s"; done
    echo "  (ensure ~/.local/bin is on PATH)"
else
    echo "universalmutator already installed: $(command -v mutate)"
fi

# --- comby (structural matcher for `mode = \"comby\"` packs) -------------------
if ! command -v comby >/dev/null 2>&1; then
    echo "Installing comby..."
    if command -v paru >/dev/null 2>&1; then
        paru -S --needed comby-bin || paru -S --needed comby
    else
        # The prebuilt comby links against libev/libpcre; install them first on
        # apt-based systems (e.g. Ubuntu CI).
        if command -v apt-get >/dev/null 2>&1; then
            sudo apt-get update && sudo apt-get install -y libpcre3 libev4
        fi
        bash <(curl -sL get.comby.dev) || {
            echo "comby install failed; see https://github.com/comby-tools/comby/releases" >&2
            exit 1
        }
    fi
else
    echo "comby already installed: $(command -v comby)"
fi

echo
echo "Done. Verify with: command -v mutate analyze_mutants comby"

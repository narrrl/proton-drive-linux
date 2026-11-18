#!/usr/bin/env bash
# Retakes the README and website screenshots from made-up data.
set -Eeuo pipefail

usage() {
    cat >&2 <<'EOF'
usage: scripts/screenshots.sh [OPTION ...]

Builds the app, starts it against a stand-in daemon that serves an invented
account (crates/pdfs-core/examples/demo_daemon.rs), and captures every page in
a headless Wayland compositor. No Proton account, no FUSE mount, nothing from
your own files: HOME, every XDG directory, the session bus and the keyring
are throwaway ones under a temporary directory.

Options:
  --out DIR        where the PNGs go (default: images/)
  --only LIST      capture only these, comma-separated (default: all of
                   login,files,photos,photos_map,computers,sync,conflicts,
                   prompt_gtk,prompt_fzf). preferences_sync and
                   preferences_apps exist too, for pull requests only
  --website DIR    afterwards, copy them into the website checkout at DIR
                   with its scripts/sync-screenshots.sh
  --settle SECS    how long a page gets to load before its capture (default 3)
  --debug          use a debug build instead of a release build
  --skip-build     reuse binaries from the selected profile
  --keep           keep the temporary directory, for a look at the logs

Needs cage, grim, wlr-randr, ImageMagick, dbus-run-session and
gnome-keyring-daemon; prompt_gtk types its query with wtype when there is one,
and prompt_fzf needs fzf and alacritty. Demo photos come from picsum.photos
(Unsplash licence) on the first run and are cached in
~/.cache/pdfs-screenshots.
EOF
    exit 2
}

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
all=login,files,photos,photos_map,computers,sync,conflicts,prompt_gtk,prompt_fzf
# Not in the README, so not taken by default.
extra=preferences_sync,preferences_apps

# The capture itself, run by cage inside the throwaway session (see below).
if [[ "${1:-}" == --capture ]]; then
    shots=",$SHOTS,"
    wants() { [[ "$shots" == *",$1,"* ]]; }
    output() { wlr-randr --output "$(wlr-randr | awk 'NR==1 {print $1}')" --custom-mode "$1"; }
    shoot() {
        sleep "$SETTLE"
        # Cold GTK/portal startup can leave the compositor black briefly.
        # Wait for actual window content rather than publishing a blank PNG.
        local capture_try
        for capture_try in 1 2 3 4 5; do
            grim "$OUT/$1.png"
            if [[ "$(magick "$OUT/$1.png" -format '%[fx:standard_deviation>0.001]' info:)" == 1 ]]; then
                echo "captured $1"
                return
            fi
            sleep 1
        done
        echo "no window content for $1; screenshot is blank" >&2
        return 1
    }
    # Cut the compositor's black background off a window smaller than it.
    trim() {
        magick "$OUT/$1.png" -bordercolor black -border 1 -trim +repage "$OUT/$1.png"
    }
    quit() {
        kill "$@" 2>/dev/null || true
        wait "$@" 2>/dev/null || true
    }

    output 1836x1006
    if wants login; then
        "$BIN/pdfs-app" &
        app=$!
        shoot login
        quit "$app"
    fi
    "$BIN/demo_daemon" seed-keyring
    "$BIN/pdfs-app" &
    app=$!
    sleep "$SETTLE"
    for shot in files:browser photos:gallery photos_map:places-map computers:devices \
        sync:sync-folders conflicts:activity \
        preferences_sync:preferences-sync preferences_apps:preferences-apps; do
        name=${shot%%:*}
        wants "$name" || continue
        "$BIN/pdfs-app" --page "${shot#*:}"
        shoot "$name"
    done
    quit "$app"

    if wants prompt_gtk; then
        output 860x723
        "$BIN/pdfs-prompt" --gtk &
        prompt=$!
        sleep 1
        # The window has no --query; type one, as a person would.
        # The first key only sets up the keymap.
        command -v wtype >/dev/null && wtype -k Shift_L -s 200 report
        shoot prompt_gtk
        quit "$prompt"
        trim prompt_gtk
    fi
    if wants prompt_fzf; then
        if command -v fzf >/dev/null && command -v alacritty >/dev/null; then
            output 800x600
            alacritty -o font.size=11 -e "$BIN/pdfs-prompt" --fzf --inner --query report &
            term=$!
            shoot prompt_fzf
            quit "$term"
            trim prompt_fzf
        else
            echo "skipped prompt_fzf: needs fzf and alacritty" >&2
        fi
    fi
    exit 0
fi

# Inside the private session bus: an unlocked, empty keyring, the demo daemon,
# then the compositor running the capture above.
if [[ "${1:-}" == --session ]]; then
    # A password it never asks for again: an empty one leaves no keyring.
    printf demo | gnome-keyring-daemon --unlock --components=secrets >/dev/null
    "$BIN/demo_daemon" serve "$PHOTOS" 2>"$PDFS_DEMO_ROOT/demo_daemon.log" &
    daemon=$!
    trap 'kill "$daemon" 2>/dev/null || true' EXIT
    for _ in $(seq 50); do
        [[ -S "$XDG_STATE_HOME/proton-drive-linux/control.sock" ]] && break
        sleep 0.1
    done
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
        cage -- "$repo/scripts/screenshots.sh" --capture 2>"$PDFS_DEMO_ROOT/cage.log"
    exit 0
fi

out="$repo/images"
shots=$all
website=
settle=3
profile=release
skip_build=
keep=
while (($#)); do
    case "$1" in
        --out) out=$(realpath -m -- "${2:?}"); shift ;;
        --only) shots=${2:?}; shift ;;
        --website) website=$(realpath -- "${2:?}"); shift ;;
        --settle) settle=${2:?}; shift ;;
        --debug) profile=debug ;;
        --skip-build) skip_build=1 ;;
        --keep) keep=1 ;;
        -h | --help) usage ;;
        *) echo "unknown option: $1" >&2; usage ;;
    esac
    shift
done
for shot in ${shots//,/ }; do
    [[ ",$all,$extra," == *",$shot,"* ]] || { echo "no screenshot called $shot" >&2; exit 2; }
done
for tool in cage grim wlr-randr magick dbus-run-session gnome-keyring-daemon cargo curl; do
    command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 2; }
done

# Demo photos, fetched once. Fixed picsum ids keep every run the same; a
# trailing -p marks a portrait one.
photos="${XDG_CACHE_HOME:-$HOME/.cache}/pdfs-screenshots"
mkdir -p "$photos"
for id in 10 11 13 15 16 17 28 29 37 42 43 49 54 57 58 76 82 83 84 96 101 103 110 \
    111 116 119 122 128 129 135 142 145 152 164 167 174 176 180 184 188 192 200 \
    210 218 219 225 231 237; do
    size=600/400 file="$photos/$id.jpg"
    if ((id % 7 == 0)); then size=400/600 file="$photos/$id-p.jpg"; fi
    [[ -s "$file" ]] || curl -fsSL "https://picsum.photos/id/$id/$size.jpg" -o "$file" ||
        { rm -f "$file"; echo "could not fetch demo photo $id; skipped" >&2; }
done

cargo_flags=(--manifest-path "$repo/Cargo.toml" --locked)
[[ "$profile" == release ]] && cargo_flags+=(--release)
if [[ -z "$skip_build" ]]; then
    cargo build "${cargo_flags[@]}" -p pdfs-gui --bin pdfs-app --bin pdfs-prompt
    cargo build "${cargo_flags[@]}" -p pdfs-core --example demo_daemon
fi
target=$(cargo metadata --manifest-path "$repo/Cargo.toml" --format-version 1 --no-deps |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
bin="$(mktemp -d)"
for executable in "$target/$profile/pdfs-app" "$target/$profile/pdfs-prompt" \
    "$target/$profile/examples/demo_daemon"; do
    [[ -x "$executable" ]] || { echo "missing executable: $executable; run without --skip-build" >&2; exit 2; }
done
ln -s "$target/$profile/pdfs-app" "$target/$profile/pdfs-prompt" \
    "$target/$profile/examples/demo_daemon" "$bin/"

root=$(mktemp -d -t pdfs-screenshots.XXXXXX)
cleanup() {
    rm -rf "$bin"
    if [[ -n "$keep" ]]; then echo "kept $root" >&2; else rm -rf "$root"; fi
}
trap cleanup EXIT
mkdir -p "$root"/{home,config,state,cache,data} "$root/run" "$out"
chmod 700 "$root/run"
# The user's own fonts, so the shots look like the app does here.
mkdir -p "$root/data/fonts"
[[ -d "$HOME/.local/share/fonts" ]] && cp -rs "$HOME/.local/share/fonts/." "$root/data/fonts/"
# Prefer this checkout's brand over an older icon from an installed package.
install -Dm644 "$repo/packaging/io.narl.proton-drive-linux.svg" \
    "$root/data/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg"
mkdir -p "$root/config/proton-drive-linux"
cat >"$root/config/proton-drive-linux/config.json" <<'EOF'
{
  "proton_theme": true,
  "tray_hidden": true,
  "update_check": false,
  "online_map": false,
  "files_view": { "list": false },
  "ignore_patterns": [".git/", "node_modules/", "target/", "*~", "*.swp", ".DS_Store",
                      "*.iso", "build/", "*.sqlite-wal"],
  "open_with": {
    "rules": [
      { "match": ["@text", "*.md"], "command": ["nvim"], "terminal": true },
      { "match": ["@image"], "command": ["loupe"] }
    ]
  }
}
EOF

# Only a close button, as GNOME shows it.
mkdir -p "$root/config/gtk-4.0"
printf '[Settings]\ngtk-decoration-layout=:close\n' >"$root/config/gtk-4.0/settings.ini"

# The demo user's home, so paths read "Home / …" for them, not for you. A
# folder that does not exist takes no writes; everything goes to XDG_*.
home=/home/alex
[[ -e "$home" ]] && home="$root/home"

outer_bus=${DBUS_SESSION_BUS_ADDRESS:-unset}
env -i \
    PATH="$PATH" LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 TZ="${TZ:-Europe/Berlin}" \
    HOME="$home" \
    XDG_CONFIG_HOME="$root/config" XDG_STATE_HOME="$root/state" \
    XDG_CACHE_HOME="$root/cache" XDG_DATA_HOME="$root/data" \
    XDG_RUNTIME_DIR="$root/run" \
    PDFS_DEMO_ROOT="$root" PDFS_DEMO_OUTER_BUS="$outer_bus" \
    GSETTINGS_BACKEND=memory ADW_DEBUG_COLOR_SCHEME=prefer-dark GTK_A11Y=none GDK_DEBUG=no-portals \
    BIN="$bin" PHOTOS="$photos" OUT="$out" SHOTS="$shots" SETTLE="$settle" \
    dbus-run-session -- "$repo/scripts/screenshots.sh" --session 2>"$root/session.log"

for shot in ${shots//,/ }; do
    [[ -s "$out/$shot.png" ]] || echo "missing: $out/$shot.png (logs: --keep)" >&2
done
if [[ -n "$website" ]]; then
    "$website/scripts/sync-screenshots.sh" "$repo"
fi

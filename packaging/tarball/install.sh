#!/usr/bin/env bash
# Install Proton Drive for Linux from this release tarball into your home
# directory, or remove it again. Needs no root, so it works on distributions
# where the system is read-only, as long as GTK 4, libadwaita, WebKitGTK 6 and
# FUSE 3 are on the system.
set -Eeuo pipefail

usage() {
    cat >&2 <<'EOF'
usage: ./install.sh [--prefix DIR] [--no-service] [--force]
       ./install.sh --uninstall [--prefix DIR] [--no-service]

Installs the app, the tray, the search launcher and the `pdfs` command into
~/.local (bin/, share/), the background service into ~/.config/systemd/user and
the tray's login entry into ~/.config/autostart. Run it again from a newer
tarball to update; the running service restarts on the new version.

  --prefix DIR   install under DIR instead of ~/.local
  --uninstall    remove what an earlier run installed; your settings, cache
                 and the files in Proton Drive stay
  --no-service   leave systemd alone (no reload, restart or stop)
  --force        install although libraries are missing or a package
                 already installs Proton Drive for Linux
EOF
    exit 2
}

prefix=""
uninstall=0
service=1
force=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix) [[ $# -ge 2 ]] || usage; prefix="$2"; shift ;;
        --prefix=*) prefix="${1#*=}" ;;
        --uninstall) uninstall=1 ;;
        --no-service) service=0 ;;
        --force) force=1 ;;
        -h | --help) usage ;;
        *) echo "unknown option: $1" >&2; usage ;;
    esac
    shift
done
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The copy kept for --uninstall knows where it was installed.
if [[ -z "$prefix" && -f "$here/installed-files" ]]; then
    prefix="$(dirname "$(dirname "$here")")"
fi
prefix="${prefix:-$HOME/.local}"
[[ "$prefix" == /* ]] || prefix="$PWD/$prefix"
prefix="${prefix%/}"

config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
unit_dir="$config_home/systemd/user"
autostart_dir="$config_home/autostart"
data_dir="$prefix/share/proton-drive-linux"
manifest="$data_dir/installed-files"
unit=proton-drive.service

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

systemctl_user() {
    [[ $service -eq 1 ]] || return 0
    command -v systemctl >/dev/null || return 0
    systemctl --user "$@" 2>/dev/null || true
}

# Remove every file listed in the manifest `$1` that is not in the list `$2`
# (one path per line), then the directories that leaves empty.
remove_listed() {
    local list="$1" keep="${2:-}" path
    [[ -f "$list" ]] || return 0
    while IFS= read -r path; do
        [[ -n "$path" ]] || continue
        if [[ -n "$keep" ]] && grep -Fxq -- "$path" <<<"$keep"; then
            continue
        fi
        rm -f -- "$path"
        rmdir -p --ignore-fail-on-non-empty -- "$(dirname -- "$path")" 2>/dev/null || true
    done <"$list"
}

if [[ $uninstall -eq 1 ]]; then
    [[ -f "$manifest" ]] || die "nothing installed under $prefix (no $manifest)"
    # Stopping unmounts My files; queued changes stay and upload on the next start.
    systemctl_user disable --now "$unit"
    remove_listed "$manifest"
    rm -f -- "$manifest" "$data_dir/install.sh"
    rmdir --ignore-fail-on-non-empty -- "$data_dir" 2>/dev/null || true
    systemctl_user daemon-reload
    say "Removed Proton Drive for Linux from $prefix."
    say "Your settings, cache and sign-in stay; see the uninstall section of docs/INSTALL.md to remove them too."
    exit 0
fi

[[ -x "$here/bin/pdfs" ]] || die "run install.sh from the unpacked tarball (no bin/pdfs next to it)"

# A package and this install would each bring a service unit, and the one in
# ~/.config wins: the two would run different versions against one database.
if [[ $force -eq 0 && ( -e /usr/bin/pdfs || -e /usr/lib/systemd/user/$unit ) ]]; then
    die "a package already installs Proton Drive for Linux (/usr/bin/pdfs). Remove it first, or keep using it."
fi

# Missing libraries would only show when the app does not start.
missing="$(for exe in pdfs pdfs-app pdfs-tray pdfs-prompt; do
    ldd "$here/bin/$exe" 2>&1 | grep -E 'not found' || true
done | sort -u)"
if [[ -n "$missing" ]]; then
    printf '%s\n' "$missing" | sed 's/^[[:space:]]*/  /' >&2
    if grep -q 'GLIBC_' <<<"$missing"; then
        warn "these binaries need glibc 2.39 or newer (Ubuntu 24.04, Debian 13, Fedora 40)"
    fi
    cat >&2 <<'EOF'
Install the missing libraries, then run this again:
  Fedora, Fedora Atomic  gtk4 libadwaita webkitgtk6.0 fuse3
                         (on Silverblue and its siblings, layer the COPR package
                         instead: see docs/INSTALL.md)
  Debian, Ubuntu         libgtk-4-1 libadwaita-1-0 libwebkitgtk-6.0-4 fuse3
  Arch Linux             gtk4 libadwaita webkitgtk-6.0 fuse3
  openSUSE               libgtk-4-1 libadwaita-1-0 libwebkitgtk-6_0-4 fuse3
EOF
    [[ $force -eq 1 ]] || die "missing libraries (--force installs anyway)"
fi

fusermount="$(command -v fusermount3 || true)"
if [[ -z "$fusermount" ]]; then
    warn "fusermount3 is missing; install fuse3, or My files cannot be mounted"
elif [[ ! -u "$fusermount" ]]; then
    warn "$fusermount is not setuid root; My files may not mount"
fi

# Where each file of the tarball goes, as "source<TAB>target" lines.
targets() {
    local file rel
    while IFS= read -r -d '' file; do
        rel="${file#"$here"/}"
        case "$rel" in
            bin/* | share/*) printf '%s\t%s\n' "$rel" "$prefix/$rel" ;;
            lib/systemd/user/*) printf '%s\t%s\n' "$rel" "$unit_dir/${rel##*/}" ;;
            etc/xdg/autostart/*) printf '%s\t%s\n' "$rel" "$autostart_dir/${rel##*/}" ;;
        esac
    done < <(find "$here/bin" "$here/share" "$here/lib" "$here/etc" -type f -print0 | sort -z)
}

# A desktop entry and a unit file take a quoted path; double quotes and
# backslashes inside it are escaped.
quoted() {
    local path="${1//\\/\\\\}"
    printf '"%s"' "${path//\"/\\\"}"
}

new_files=""
while IFS=$'\t' read -r rel target; do
    mode=644
    [[ "$rel" == bin/* ]] && mode=755
    install -Dm"$mode" -- "$here/$rel" "$target"
    case "$rel" in
        share/applications/*.desktop | etc/xdg/autostart/*.desktop)
            exe="$(sed -n 's/^Exec=\([^ ]*\).*/\1/p' "$target" | head -n1)"
            sed -i "s|^Exec=$exe|Exec=$(quoted "$prefix/bin/$exe" | sed 's/[&|]/\\&/g')|" "$target"
            ;;
        lib/systemd/user/*)
            sed -i "s|^ExecStart=/usr/bin/pdfs|ExecStart=$(quoted "$prefix/bin/pdfs" | sed 's/[&|]/\\&/g')|" "$target"
            ;;
    esac
    new_files+="$target"$'\n'
done < <(targets)

# Drop what an older version installed and this one does not.
remove_listed "$manifest" "$new_files"
mkdir -p -- "$data_dir"
printf '%s' "$new_files" >"$manifest"
install -m755 -- "$here/install.sh" "$data_dir/install.sh"

if command -v gtk-update-icon-cache >/dev/null &&
    [[ -f "$prefix/share/icons/hicolor/index.theme" ]]; then
    gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" 2>/dev/null || true
fi

systemctl_user daemon-reload
if [[ $service -eq 1 ]] && command -v systemctl >/dev/null &&
    systemctl --user is-active -q "$unit" 2>/dev/null; then
    systemctl --user restart "$unit" && say "Restarted the running service on the new version."
fi

say "Installed Proton Drive for Linux $("$prefix/bin/pdfs" --version 2>/dev/null | awk '{print $NF}') into $prefix."
say "Open \"Proton Drive for Linux\" from your applications menu and sign in; the app starts the service."
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) say "To use \`pdfs\` in a terminal, add $prefix/bin to your PATH." ;;
esac
say "To update, run install.sh from a newer tarball. To remove: $data_dir/install.sh --uninstall"

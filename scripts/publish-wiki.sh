#!/usr/bin/env bash
# Publish the user documentation in docs/ to the GitHub wiki.
#
# The pages in docs/ are the source of truth; the wiki is a generated copy for
# people who look there first. Each run clones the wiki, replaces the pages
# listed below, rewrites links between them to wiki page names and every other
# repository link to an absolute GitHub URL, regenerates Home and _Sidebar, and
# commits. Nothing is pushed without --push.
#
# GitHub creates the wiki repository only when the first page is saved, so
# create any page once in the web UI before the first run.
#
# usage: scripts/publish-wiki.sh [--push] [WORKDIR]
set -euo pipefail

repo_url="https://github.com/narrrl/proton-drive-linux"
wiki_remote="git@github.com:narrrl/proton-drive-linux.wiki.git"

push=0
if [ "${1:-}" = "--push" ]; then
  push=1
  shift
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="${1:-$(mktemp -d)}"
wiki="$workdir/wiki"

if [ -d "$wiki/.git" ]; then
  git -C "$wiki" pull --ff-only --quiet
else
  git clone --quiet "$wiki_remote" "$wiki"
fi

rev="$(git -C "$root" rev-parse --short HEAD)"
version="$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)"

python3 - "$root" "$wiki" "$repo_url" "$version" <<'EOF'
import pathlib
import re
import sys

root, wiki, repo_url, version = sys.argv[1:]
root = pathlib.Path(root)
wiki = pathlib.Path(wiki)

# docs/ file -> wiki page name, in sidebar order.
PAGES = {
    "INSTALL.md": "Installation",
    "USER_GUIDE.md": "User-Guide",
    "CLI.md": "Command-Line-Reference",
    "CONFIGURATION.md": "Configuration",
    "TROUBLESHOOTING.md": "Troubleshooting",
    "RECOVERY.md": "Recovery",
}

LINK = re.compile(r"(\]\()([^)\s]+)(\))")


def rewrite(target: str) -> str:
    if re.match(r"^[a-z]+:|^#", target):
        return target
    path, _, anchor = target.partition("#")
    anchor = f"#{anchor}" if anchor else ""
    if path in PAGES:
        return PAGES[path] + anchor
    # Anything else is a file in the repository: link to it on GitHub.
    resolved = (root / "docs" / path).resolve().relative_to(root)
    kind = "tree" if (root / resolved).is_dir() else "blob"
    return f"{repo_url}/{kind}/main/{resolved}{anchor}"


def convert(text: str) -> str:
    return LINK.sub(lambda m: m.group(1) + rewrite(m.group(2)) + m.group(3), text)


note = (
    "<!-- Generated from docs/ in the repository by scripts/publish-wiki.sh."
    " Edit the source there; changes made here are overwritten. -->\n\n"
)

for source, page in PAGES.items():
    text = (root / "docs" / source).read_text()
    (wiki / f"{page}.md").write_text(note + convert(text))


def title(source: str) -> str:
    return (root / "docs" / source).read_text().splitlines()[0].lstrip("# ").strip()


home = [
    note,
    "# Proton Drive for Linux\n\n",
    "An unofficial Proton Drive client for the Linux desktop: your files as a folder that ",
    "downloads on demand, synced local folders, Photos, sharing, a GTK4 app with tray and search ",
    "launcher, and a scriptable CLI.\n\n",
    f"This wiki mirrors the documentation of version {version}. The source, releases and issue ",
    f"tracker are in the [repository]({repo_url}).\n\n",
    "## Pages\n\n",
]
home += [f"- [{title(source)}]({page})\n" for source, page in PAGES.items()]
home += [
    "\n## More\n\n",
    f"- [Changelog]({repo_url}/blob/main/docs/CHANGELOG.md)\n",
    f"- [Architecture]({repo_url}/blob/main/docs/ARCHITECTURE.md)\n",
    f"- [Contributing]({repo_url}/blob/main/CONTRIBUTING.md)\n",
    f"- [Report a problem]({repo_url}/issues/new/choose)\n",
]
(wiki / "Home.md").write_text("".join(home))

sidebar = ["**[Home](Home)**\n\n"]
sidebar += [f"- [{title(source)}]({page})\n" for source, page in PAGES.items()]
sidebar += [
    "\n---\n\n",
    f"[Releases]({repo_url}/releases) · [Issues]({repo_url}/issues)\n",
]
(wiki / "_Sidebar.md").write_text("".join(sidebar))
EOF

cd "$wiki"
git add -A
if git diff --cached --quiet; then
  echo "Wiki is already up to date."
  exit 0
fi
git commit --quiet -m "Update from docs at $rev"
git show --stat --format='%s' HEAD

if [ "$push" = 1 ]; then
  git push --quiet
  echo "Pushed to $wiki_remote."
else
  echo "Committed in $wiki. Review it, then push from there or re-run with --push."
fi

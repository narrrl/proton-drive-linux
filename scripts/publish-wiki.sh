#!/usr/bin/env bash
# Publish the documentation in docs/ to the GitHub wiki.
#
# The pages in docs/ are the source of truth; the wiki is a generated copy for
# people who look there first. Each run checks every link in docs/, clones the
# wiki, replaces all of its pages with the ones listed in SECTIONS below,
# rewrites links between them to wiki page names and every other repository
# link to an absolute GitHub URL, copies the images they show, writes the
# sidebar and footer, and commits. Nothing is pushed without --push.
#
# --check only checks the links and that every page in docs/ is listed or
# excluded, without touching the wiki. CI runs it for every change to docs/.
#
# GitHub creates the wiki repository only when the first page is saved, so
# create any page once in the web UI before the first run. WIKI_REMOTE
# overrides the remote, which is how the workflow pushes with its token.
#
# usage: scripts/publish-wiki.sh [--check | --push] [WORKDIR]
set -euo pipefail

repo_url="https://github.com/narrrl/proton-drive-linux"
wiki_remote="${WIKI_REMOTE:-git@github.com:narrrl/proton-drive-linux.wiki.git}"

mode=commit
case "${1:-}" in
  --check) mode=check; shift ;;
  --push) mode=push; shift ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="${1:-$(mktemp -d)}"
wiki="$workdir/wiki"

if [ "$mode" = check ]; then
  wiki="$workdir/check"
  mkdir -p "$wiki"
elif [ -d "$wiki/.git" ]; then
  git -C "$wiki" pull --ff-only --quiet
else
  git clone --quiet "$wiki_remote" "$wiki"
fi

rev="$(git -C "$root" rev-parse --short HEAD)"
version="$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)"

python3 - "$root" "$wiki" "$repo_url" "$version" "$rev" <<'EOF'
import pathlib
import re
import shutil
import sys

root, wiki, repo_url, version, rev = sys.argv[1:]
root = pathlib.Path(root)
wiki = pathlib.Path(wiki)
docs = root / "docs"

HOME = ("README.md", "Home")

# Sidebar sections: docs/ file -> wiki page name, in order.
SECTIONS = [
    ("Start here", [
        ("GETTING_STARTED.md", "Getting-Started"),
        ("INSTALL.md", "Installation"),
        ("FAQ.md", "FAQ"),
    ]),
    ("Using it", [
        ("USER_GUIDE.md", "User-Guide"),
        ("FILES.md", "Files-and-Offline"),
        ("SYNCED_FOLDERS.md", "Synced-Folders"),
        ("PHOTOS.md", "Photos"),
        ("SHARING.md", "Sharing"),
        ("COMPUTERS.md", "Computers"),
        ("LAUNCHER_AND_TRAY.md", "Launcher-and-Tray"),
    ]),
    ("Reference", [
        ("CLI.md", "Command-Line-Reference"),
        ("CONFIGURATION.md", "Configuration"),
        ("SECURITY_MODEL.md", "Security-Model"),
    ]),
    ("When something goes wrong", [
        ("TROUBLESHOOTING.md", "Troubleshooting"),
        ("RECOVERY.md", "Recovery"),
    ]),
    ("Contributing", [
        ("DEVELOPMENT.md", "Development"),
        ("ARCHITECTURE.md", "Architecture"),
        ("TESTING.md", "Testing"),
        ("TRANSLATING.md", "Translating"),
        ("ROADMAP.md", "Roadmap"),
    ]),
]

# Too long or too internal for the wiki; links to them go to GitHub.
EXCLUDED = {"BUGS.md", "CHANGELOG.md"}

# Files outside docs/ whose links are checked but not published.
CHECKED = ["README.md", "CONTRIBUTING.md", "SECURITY.md"]

PAGES = dict([HOME] + [page for _, pages in SECTIONS for page in pages])
images_url = f"https://raw.githubusercontent.com/wiki/{repo_url.removeprefix('https://github.com/')}"

LINK = re.compile(r"(\]\(|src=\")([^)\"\s]+)([)\"])")
FENCE = re.compile(r"^\s*(```|~~~)")
INLINE_CODE = re.compile(r"(`[^`\n]*`)")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
EXPLICIT_ID = re.compile(r"<a\s+id=\"([^\"]+)\"")

errors = []
images = set()


def outside_fences(text):
    """Yield (is_prose, chunk) so links in code blocks are left alone."""
    chunk, fenced = [], False
    for line in text.splitlines(keepends=True):
        if FENCE.match(line):
            if chunk:
                yield not fenced, "".join(chunk)
            chunk, fenced = [], not fenced
            yield False, line
            continue
        chunk.append(line)
    if chunk:
        yield not fenced, "".join(chunk)


def slug(heading):
    """GitHub's anchor for a heading."""
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", heading)
    return re.sub(r"[^\w\- ]", "", text.lower()).replace(" ", "-")


anchor_cache = {}


def anchors(path):
    if path not in anchor_cache:
        found, seen = set(), {}
        for prose, chunk in outside_fences(path.read_text()):
            if not prose:
                continue
            for line in chunk.splitlines():
                found.update(EXPLICIT_ID.findall(line))
                m = HEADING.match(line)
                if not m:
                    continue
                base = slug(m.group(2))
                n = seen.get(base, 0)
                seen[base] = n + 1
                found.add(base if n == 0 else f"{base}-{n}")
        anchor_cache[path] = found
    return anchor_cache[path]


def rewrite(source, target):
    """The wiki form of one link in source, recording broken links."""
    if re.match(r"^[a-z]+:", target):
        return target
    path, _, anchor = target.partition("#")
    resolved = (source.parent / path).resolve() if path else source
    where = f"{source.relative_to(root)}: {target}"
    if not resolved.is_relative_to(root) or not resolved.exists():
        errors.append(f"{where}: no such file")
        return target
    if anchor and resolved.suffix == ".md" and anchor not in anchors(resolved):
        errors.append(f"{where}: no heading #{anchor}")
    relative = resolved.relative_to(root)
    anchor = f"#{anchor}" if anchor else ""
    if not path:
        return anchor
    if relative.parent == pathlib.Path("docs") and relative.name in PAGES:
        return PAGES[relative.name] + anchor
    if relative.parts[0] == "images":
        images.add(relative)
        return f"{images_url}/{relative}"
    kind = "tree" if resolved.is_dir() else "blob"
    return f"{repo_url}/{kind}/main/{relative}{anchor}"


def convert(source):
    out = []
    for prose, chunk in outside_fences(source.read_text()):
        if prose:
            # Odd pieces are `inline code`, which is left alone too.
            pieces = INLINE_CODE.split(chunk)
            pieces[::2] = [
                LINK.sub(lambda m: m.group(1) + rewrite(source, m.group(2)) + m.group(3), p)
                for p in pieces[::2]
            ]
            chunk = "".join(pieces)
        out.append(chunk)
    return "".join(out)


def title(source):
    return (docs / source).read_text().splitlines()[0].lstrip("# ").strip()


def without_title(text):
    lines = text.splitlines(keepends=True)
    if lines and lines[0].startswith("# "):
        lines = lines[1:]
        while lines and not lines[0].strip():
            lines = lines[1:]
    return "".join(lines)


for name in sorted(p.name for p in docs.glob("*.md")):
    if name not in PAGES and name not in EXCLUDED:
        errors.append(f"docs/{name}: add it to SECTIONS or EXCLUDED in scripts/publish-wiki.sh")

converted = {source: convert(docs / source) for source in PAGES}
published_images = set(images)
for name in CHECKED:
    convert(root / name)

if errors:
    print("Broken documentation links:", file=sys.stderr)
    for error in errors:
        print(f"  {error}", file=sys.stderr)
    sys.exit(1)

# The wiki is generated: anything not written below is stale.
for old in wiki.glob("*.md"):
    old.unlink()
shutil.rmtree(wiki / "images", ignore_errors=True)

note = (
    "<!-- Generated from docs/ in the repository by scripts/publish-wiki.sh."
    " Edit the source there; changes made here are overwritten. -->\n\n"
)
for source, page in PAGES.items():
    (wiki / f"{page}.md").write_text(note + without_title(converted[source]))

for image in published_images:
    (wiki / image).parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(root / image, wiki / image)

sidebar = ["**[Home](Home)**\n"]
for section, pages in SECTIONS:
    sidebar.append(f"\n**{section}**\n\n")
    sidebar += [f"- [{title(source)}]({page})\n" for source, page in pages]
sidebar += [
    "\n---\n\n",
    f"[Changelog]({repo_url}/blob/main/docs/CHANGELOG.md) · ",
    f"[Releases]({repo_url}/releases) · [Issues]({repo_url}/issues)\n",
]
(wiki / "_Sidebar.md").write_text(note + "".join(sidebar))

(wiki / "_Footer.md").write_text(
    note
    + f"Documentation of Proton Drive for Linux {version}, generated from "
    + f"[`docs/`]({repo_url}/tree/{rev}/docs) at `{rev}`. "
    + f"To change a page, edit it in the [repository]({repo_url}).\n"
)
EOF

if [ "$mode" = check ]; then
  echo "Documentation links are fine."
  exit 0
fi

cd "$wiki"
git add -A
if git diff --cached --quiet; then
  echo "Wiki is already up to date."
  exit 0
fi
git commit --quiet -m "Update from docs at $rev"
git show --stat --format='%s' HEAD

if [ "$mode" = push ]; then
  git push --quiet
  echo "Pushed to the wiki."
else
  echo "Committed in $wiki. Review it, then push from there or re-run with --push."
fi

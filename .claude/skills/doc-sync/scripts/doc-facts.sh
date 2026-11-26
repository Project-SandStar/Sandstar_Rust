#!/usr/bin/env bash
# Print the facts documents quote (version, tag, deployed build, test count)
# next to the lines that quote them, so stale ones stand out.
# Read-only. Run from the repository root:
#   bash .claude/skills/doc-sync/scripts/doc-facts.sh
set -u
cd "$(git rev-parse --show-toplevel)" || exit 1

version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
# Highest version tag. Release tags sit on master merge commits, so
# `git describe` on develop would miss the newest one.
tag=$(git tag --sort=-v:refname | head -1)
tag=${tag:-"(no tag)"}
ahead=$(git rev-list --count "$tag"..HEAD 2>/dev/null || echo "?")

echo "== Current values =="
echo "Cargo.toml version : $version"
echo "Highest tag        : $tag   (HEAD has $ahead commit(s) not in it)"
echo "Branch / HEAD      : $(git branch --show-current) $(git log -1 --format=%h)"
echo "Test count         : not measured here; use passed= from the last 'bash tools/gates.sh'"
echo "Deployed build     : not known here; read /api/about on the device after a deploy"
echo

show() { # title, pattern, files...
    local title=$1 pat=$2
    shift 2
    echo "== $title =="
    grep -n -E -i "$pat" "$@" 2>/dev/null | cut -c1-220 || true
    echo
}

show "Version lines" \
    '\*\*Version:\*\*|Current version' \
    README.md docs/Progress/OVERVIEW.md

echo "== CHANGELOG: first three sections =="
grep -n '^## \[' CHANGELOG.md | head -3
echo

echo "== SECURITY.md supported versions =="
grep -n -E '^\| *[0-9<>=]' SECURITY.md | head -5
echo

show "Deployed-build lines" \
    'Deployed:|\*\*Production:\*\*' \
    README.md docs/Progress/OVERVIEW.md

show "Test-count lines" \
    '[0-9],?[0-9]{3} (tests|passing|passed)|\*\*Tests:\*\*' \
    README.md docs/Progress/OVERVIEW.md

show "Version patterns in the deploy checklist" \
    '[0-9]+\.[0-9]+\.x|sandstar_[0-9]' \
    docs/Progress/DEPLOYMENT_CHECKLIST.md

if [ -d docs/DevelopingMds ]; then
    echo "== DevelopingMds (not in git) =="
    echo "08 last step heading : $(grep -n '^### S-' docs/DevelopingMds/08-ILERLEME.md | tail -1 | cut -c1-120)"
    echo "10 last step line    : $(grep -n '^- .*\*\*S-[0-9]' docs/DevelopingMds/10-KALDIGIMIZ-YER.md | tail -1 | cut -c1-120)"
    echo "06 'commit bekliyor' : $(grep -c 'commit bekliyor' docs/DevelopingMds/06-RISKLER.md) line(s)"
    echo "Plans still open     :"
    grep -l -E '^\*\*Durum:\*\* .*(ONAY BEKLİYOR|commit bekliyor)' docs/DevelopingMds/Planlar/*.md 2>/dev/null | sed 's/^/  /'
fi

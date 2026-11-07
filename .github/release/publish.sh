#!/bin/sh
# Publishes the GitHub Release for the tag $1 with every file in the directory $2: the archives
# and SHA256SUMS. release.yml's publish job runs it with GH_TOKEN and GITHUB_REPOSITORY; an xtask
# test (release::tests) runs it against a fake gh.
#
# A run can resume a release an earlier attempt left unfinished. It creates the release, as a
# draft, only when the tag has none; uploads only the files the release lacks; never replaces an
# asset, and refuses one whose digest is not that of this run's file; and publishes the draft
# last, so nobody sees a release without all its assets.
set -eu

tag=$1
cd "$2"

# Every file is the one SHA256SUMS lists, and SHA256SUMS lists every file.
shasum -a 256 --check --status SHA256SUMS
for file in *; do
  if [ "$file" != SHA256SUMS ] && ! awk -v file="$file" '$2 == file { found = 1 } END { exit !found }' SHA256SUMS; then
    echo "SHA256SUMS does not list $file" >&2
    exit 1
  fi
done

# The tag's releases, drafts included: a line `release <draft>` for each, and a line
# `asset <name> <digest>` for each of their assets.
state=$(TAG=$tag gh api --paginate "repos/$GITHUB_REPOSITORY/releases" --jq \
  '.[] | select(.tag_name == env.TAG) | "release \(.draft)", (.assets[] | "asset \(.name) \(.digest // "none")")')
releases=$(printf '%s\n' "$state" | grep -c '^release ' || true)
case "$releases" in
  0)
    gh release create "$tag" --repo "$GITHUB_REPOSITORY" --draft --verify-tag \
      --title "abc ${tag#v}" --generate-notes
    draft=true
    ;;
  1)
    draft=$(printf '%s\n' "$state" | sed -n 's/^release //p')
    ;;
  *)
    echo "$GITHUB_REPOSITORY has $releases releases for $tag: delete the drafts that are not the release" >&2
    exit 1
    ;;
esac
assets=$(printf '%s\n' "$state" | sed -n 's/^asset //p')

# Every asset the release holds already is this run's file, byte for byte.
while read -r name digest; do
  if [ -z "$name" ]; then
    continue
  fi
  if [ ! -f "$name" ]; then
    echo "the release $tag holds $name, which this run did not build" >&2
    exit 1
  fi
  ours="sha256:$(shasum -a 256 "$name" | cut -d ' ' -f 1)"
  if [ "$digest" != "$ours" ]; then
    echo "the release $tag holds $name with digest $digest, not this run's $ours: a published asset is never replaced. Re-run only the failed jobs of the run that uploaded it, or, while the release is a draft nobody has seen, delete it and re-run" >&2
    exit 1
  fi
done <<EOF
$assets
EOF

for file in *; do
  if printf '%s\n' "$assets" | awk -v file="$file" '$1 == file { found = 1 } END { exit !found }'; then
    echo "The release holds $file already."
  else
    gh release upload "$tag" "$file" --repo "$GITHUB_REPOSITORY"
  fi
done

if [ "$draft" = true ]; then
  gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false
fi
echo "The release $tag is published with every asset."

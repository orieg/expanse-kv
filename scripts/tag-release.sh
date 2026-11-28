#!/usr/bin/env bash
# Create the signed release tag vX.Y.Z after checking it is safe to create.
#
# release.yml publishes on the push of a version tag, and a pushed tag cannot
# be moved. This script makes, before the tag exists, the checks the release
# gate makes after the push. It creates the tag and prints the push command.
# It never pushes.
#
# Usage: scripts/tag-release.sh X.Y.Z[-pre] [--commit SHA]
#
# Creating a tag is the maintainer's action (AGENTS.md §5).
set -euo pipefail
cd "$(dirname "$0")/.."

REPO_SLUG="orieg/expanse-kv"
REMOTE="origin"
BRANCH="main"
CI_CHECK="CI Gate / All Checks Passed"

refuse() { printf 'refused: %s\n' "$*" >&2; exit 1; }

version=""
commit_given=0
commit=""
while [ $# -gt 0 ]; do
  case "$1" in
    --commit)
      [ $# -ge 2 ] || refuse "--commit needs a value"
      commit_given=1; commit="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*)
      refuse "unknown option $1" ;;
    *)
      [ -z "${version}" ] || refuse "more than one version given"
      version="$1"; shift ;;
  esac
done

[[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] \
  || refuse "'${version}' is not a version (X.Y.Z or X.Y.Z-pre)"
tag="v${version}"

# The right repository: a tag pushed anywhere else releases nothing here and
# may start another repository's tag workflows.
url="$(git remote get-url "${REMOTE}")"
case "${url%.git}" in
  "https://github.com/${REPO_SLUG}"|"git@github.com:${REPO_SLUG}"|"ssh://git@github.com/${REPO_SLUG}") ;;
  *) refuse "${REMOTE} is ${url}, not ${REPO_SLUG}" ;;
esac

git fetch --quiet "${REMOTE}" "${BRANCH}"

# The commit to tag. An empty --commit is an error, never a fallback to HEAD.
if [ "${commit_given}" -eq 1 ]; then
  [ -n "${commit// /}" ] || refuse "--commit is empty"
  ref="${commit}"
else
  ref="${REMOTE}/${BRANCH}"
fi
sha="$(git rev-parse --verify --quiet "${ref}^{commit}")" \
  || refuse "'${ref}' does not name a commit"

git merge-base --is-ancestor "${sha}" "${REMOTE}/${BRANCH}" \
  || refuse "${sha} is not on ${REMOTE}/${BRANCH}"

if git rev-parse --verify --quiet "refs/tags/${tag}" >/dev/null; then
  refuse "tag ${tag} already exists locally"
fi
remote_tags="$(git ls-remote --tags "${REMOTE}" "refs/tags/${tag}")" \
  || refuse "cannot list ${REMOTE}'s tags; not tagging blind"
[ -z "${remote_tags}" ] || refuse "tag ${tag} already exists on ${REMOTE}"

# The workspace version at that commit, read the way release.yml reads it.
have="$(git show "${sha}:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -n1)"
[ -n "${have}" ] || refuse "Cargo.toml at ${sha} declares no workspace version"
[ "${have}" = "${version}" ] \
  || refuse "the workspace version at ${sha} is ${have}, not ${version}"

# The rollup check on that commit: its newest run must have concluded success.
command -v gh >/dev/null 2>&1 || refuse "gh is not installed; cannot read the CI result for ${sha}"
ci="$(gh api "repos/${REPO_SLUG}/commits/${sha}/check-runs?per_page=100" --paginate \
        --jq ".check_runs[] | select(.name == \"${CI_CHECK}\") | [.started_at, (.conclusion // .status)] | @tsv")" \
  || refuse "cannot read the check runs for ${sha}"
conclusion="$(printf '%s\n' "${ci}" | sort | tail -n1 | cut -f2)"
[ "${conclusion}" = "success" ] \
  || refuse "'${CI_CHECK}' on ${sha} is '${conclusion:-absent}', not success"

git tag -s "${tag}" -m "expanse-kv ${version}" "${sha}"

# Read the tag back; a tag that fails is removed, since it was never pushed.
readback() {
  [ "$(git cat-file -t "refs/tags/${tag}")" = "tag" ] || { echo "${tag} is not an annotated tag"; return 1; }
  [ "$(git rev-parse "refs/tags/${tag}^{commit}")" = "${sha}" ] || { echo "${tag} does not point at ${sha}"; return 1; }
  git verify-tag "${tag}" >/dev/null 2>&1 || { echo "the signature on ${tag} does not verify"; return 1; }
}
if ! why="$(readback)"; then
  git tag -d "${tag}" >/dev/null
  refuse "${why}"
fi

printf 'created %s on %s\n\npush it with:\n\n  git push %s refs/tags/%s\n' \
  "${tag}" "${sha}" "${REMOTE}" "${tag}"

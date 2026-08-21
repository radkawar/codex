#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF' >&2
Usage: sync-upstream.sh [options]

Sync a local branch with openai/codex.

Options:
  --branch <name>           Local branch to update. Defaults to the current branch.
  --upstream-remote <name>  Remote name to use for upstream. Defaults to upstream.
  --upstream-url <url>      Upstream remote URL. Defaults to https://github.com/openai/codex.git.
  --upstream-branch <name>  Upstream branch to merge from. Defaults to main.
  --push                    Push the updated branch to origin after a successful merge.
  --push-remote <name>      Remote to push to when --push is set. Defaults to origin.
  -h, --help                Show this help text.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

info() {
  echo "==> $*" >&2
}

target_branch=""
upstream_remote="upstream"
upstream_url="https://github.com/openai/codex.git"
upstream_branch="main"
push_remote="origin"
push_after_sync=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --branch)
      [[ $# -ge 2 ]] || die "--branch requires a value"
      target_branch="$2"
      shift 2
      ;;
    --upstream-remote)
      [[ $# -ge 2 ]] || die "--upstream-remote requires a value"
      upstream_remote="$2"
      shift 2
      ;;
    --upstream-url)
      [[ $# -ge 2 ]] || die "--upstream-url requires a value"
      upstream_url="$2"
      shift 2
      ;;
    --upstream-branch)
      [[ $# -ge 2 ]] || die "--upstream-branch requires a value"
      upstream_branch="$2"
      shift 2
      ;;
    --push)
      push_after_sync=1
      shift
      ;;
    --push-remote)
      [[ $# -ge 2 ]] || die "--push-remote requires a value"
      push_remote="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

command -v git >/dev/null 2>&1 || die "git is required"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  die "run this script from inside a git repository"
}

cd "$repo_root"

git_dir="$(git rev-parse --git-dir)"
if [[ -e "$git_dir/MERGE_HEAD" || -d "$git_dir/rebase-apply" || -d "$git_dir/rebase-merge" ]]; then
  die "repository already has a merge or rebase in progress"
fi

if ! git diff --quiet --ignore-submodules -- || ! git diff --cached --quiet --ignore-submodules --; then
  die "working tree has tracked changes; commit or stash them before syncing"
fi

current_branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ -n "$current_branch" ]] || die "detached HEAD is not supported"

if [[ -z "$target_branch" ]]; then
  target_branch="$current_branch"
fi

git show-ref --verify --quiet "refs/heads/$target_branch" || {
  die "local branch does not exist: $target_branch"
}

if git remote get-url "$upstream_remote" >/dev/null 2>&1; then
  existing_upstream_url="$(git remote get-url "$upstream_remote")"
  if [[ "$existing_upstream_url" != "$upstream_url" ]]; then
    info "Updating $upstream_remote remote URL"
    git remote set-url "$upstream_remote" "$upstream_url"
  fi
else
  info "Adding $upstream_remote remote"
  git remote add "$upstream_remote" "$upstream_url"
fi

info "Fetching $upstream_remote/$upstream_branch"
git fetch "$upstream_remote" "$upstream_branch"

upstream_ref="$upstream_remote/$upstream_branch"

if [[ "$current_branch" != "$target_branch" ]]; then
  info "Switching to $target_branch"
  git switch "$target_branch"
fi

if git merge-base --is-ancestor "$upstream_ref" HEAD; then
  info "$target_branch already contains $upstream_ref"
else
  info "Merging $upstream_ref into $target_branch"
  if ! git merge --no-edit "$upstream_ref"; then
    cat <<EOF >&2
Merge conflicts detected while merging $upstream_ref into $target_branch.
Resolve them, then run:
  git add <resolved-files>
  git commit

Or abort the merge with:
  git merge --abort
EOF
    exit 1
  fi
fi

if (( push_after_sync )); then
  info "Pushing $target_branch to $push_remote"
  git push "$push_remote" "$target_branch"
fi

info "Sync complete"

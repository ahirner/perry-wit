#!/usr/bin/env bash
set -euo pipefail

branch=codex/update-dependencies
source_dir=artifacts/source

case "$1" in
  prepare)
    mkdir -p "$source_dir"
    test -z "$(git status --porcelain)"
    git config user.name 'github-actions[bot]'
    git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
    if [[ "${UPDATE_DRY_RUN:-false}" == true ]]; then
      remote=''
      main=$(git rev-parse HEAD)
    else
      remote=$(git ls-remote origin "refs/heads/$branch" | cut -f1)
      git fetch --no-tags origin refs/heads/main:refs/remotes/origin/main
      main=$(git rev-parse origin/main)
    fi
    if [[ -n "$remote" ]]; then
      git fetch --no-tags origin "$remote"
    fi
    git checkout -B "$branch" "${remote:-$main}"
    jq -n --arg remote "$remote" --arg main "$main" --arg baseline "$(git rev-parse HEAD)" \
      '{remote_head: $remote, main: $main, baseline: $baseline}' > "$source_dir/branch.json"
    if ! git merge --no-edit "$main"; then
      git merge --abort
      echo 'The update branch conflicts with main. Resolve the merge before refreshing.' >&2
      exit 1
    fi
    jq --arg baseline "$(git rev-parse HEAD)" '.baseline = $baseline' "$source_dir/branch.json" > "$source_dir/branch.tmp"
    mv "$source_dir/branch.tmp" "$source_dir/branch.json"
    ;;
  commit)
    shift
    message=$1
    shift
    git add -- "$@"
    if ! git diff --cached --quiet; then
      git commit -m "$message"
    fi
    ;;
  bundle)
    baseline=$(jq -r .baseline "$source_dir/branch.json")
    git update-ref refs/heads/update-baseline "$baseline"
    git update-ref refs/heads/update-candidate HEAD
    git bundle create "$source_dir/source.bundle" refs/heads/update-baseline refs/heads/update-candidate
    ;;
  restore)
    phase=$2
    revision=$(jq -r --arg phase "$phase" '.[$phase]' "$source_dir/metadata.json")
    [[ "$revision" =~ ^[0-9a-f]{40}$ ]]
    git fetch "$PWD/$source_dir/source.bundle" "refs/heads/update-$phase"
    git checkout --detach "$revision"
    ;;
  publish)
    : "${GH_TOKEN:?Configure GH_TOKEN_FOR_UPDATES with contents and pull-request write access}"
    expected=$(jq -r .remote_head "$source_dir/metadata.json")
    actual=$(git ls-remote origin "refs/heads/$branch" | cut -f1)
    if [[ "$actual" != "$expected" ]]; then
      echo 'The update branch changed during validation. Your commits were preserved; run the updater again.' >&2
      exit 1
    fi
    git config user.name 'github-actions[bot]'
    git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
    if [[ -d dependency-evidence ]]; then
      bash scripts/dependency_branch.sh commit 'test: retain dependency update counterexamples' dependency-evidence
    fi
    if [[ -n "$expected" ]]; then
      git merge-base --is-ancestor "$expected" HEAD
    fi
    main=$(jq -r .main "$source_dir/metadata.json")
    if [[ "$(git rev-parse HEAD)" != "$expected" ]] && { [[ -n "$expected" ]] || ! git diff --quiet "$main...HEAD"; }; then
      git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
        push "--force-with-lease=refs/heads/$branch:$expected" origin "HEAD:refs/heads/$branch"
    fi
    ready=$(jq -r .ready artifacts/comparison/comparison.json)
    pull=$(gh pr list --head "$branch" --base main --state open --json number,isDraft --jq '.[0] // empty')
    title='chore: update Nix and Rust dependencies'
    if [[ -n "$pull" ]]; then
      number=$(jq -r .number <<< "$pull")
      gh pr edit "$number" --title "$title" --body-file artifacts/comparison/summary.md
      if [[ "$ready" == "true" && "$(jq -r .isDraft <<< "$pull")" == "true" ]]; then
        gh pr ready "$number"
      elif [[ "$ready" == "false" && "$(jq -r .isDraft <<< "$pull")" == "false" ]]; then
        gh pr ready "$number" --undo
      fi
    elif ! git diff --quiet "$main...HEAD"; then
      draft=()
      if [[ "$ready" != "true" ]]; then draft=(--draft); fi
      gh pr create --head "$branch" --base main --title "$title" --body-file artifacts/comparison/summary.md "${draft[@]}"
    fi
    [[ "$ready" == "true" ]]
    ;;
  *) echo "Usage: $0 prepare | commit MESSAGE PATH... | bundle | restore PHASE | publish" >&2; exit 2 ;;
esac

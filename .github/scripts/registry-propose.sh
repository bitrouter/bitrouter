#!/usr/bin/env bash
# Fresh-run conditional updates to the managed sync proposal.
# https://git-scm.com/docs/git-push#Documentation/git-push.txt---force-with-leaseltrefnamegtltexpectgt
set -euo pipefail
current_base="$(git ls-remote origin "refs/heads/$REGISTRY_BASE" | awk '{print $1}')"
current_proposal="$(git ls-remote origin "refs/heads/$REGISTRY_BRANCH" | awk '{print $1}')"
if [ "$current_base" != "$REGISTRY_BASE_SHA" ] || [ "$current_proposal" != "$REGISTRY_PROPOSAL_SHA" ]; then
  echo '::error::Registry base or proposal changed during fetch; rerun with fresh observations.'
  exit 1
fi
pr="$(gh pr list --base "$REGISTRY_BASE" --head "$REGISTRY_BRANCH" --json number,isDraft --jq '.[0] // {}')"
pr_number="$(jq -r '.number // empty' <<< "$pr")"
paths=(registry dist/registry apps/bitrouter/registry-dist)
git add "${paths[@]}"
if git diff --cached --quiet -- "${paths[@]}"; then
  if [ -n "$pr_number" ]; then
    gh pr close "$pr_number"
    echo 'Closed superseded registry proposal; accepted content is restored.'
  else
    echo 'Registry content unchanged; no commit or PR needed.'
  fi
  exit 0
fi
if [ -n "$pr_number" ] && [ -n "$REGISTRY_PROPOSAL_SHA" ] && git diff --cached --quiet "$REGISTRY_PROPOSAL_SHA" -- "${paths[@]}"; then
  echo 'Registry proposal content unchanged; no commit or PR update needed.'
  exit 0
fi
jq -r '
  "Refresh Registry v1 from explicitly configured provider feeds.\n\nIdentity audit: " + .audit.outcome
  + "\n\nIdentity repair proposals requiring human review:\n"
  + ([.identity_repairs[] | "- " + (.kind | @json) + " for " + (.canonical_id | @json)
      + if has("provider") then " at provider " + (.provider | @json) + ", native ID " + (.provider_model_id | @json) else "" end] | join("\n"))
  + "\n\nRequired feed outcomes:\n"
  + ([.feeds[] | "- " + .provider + "/" + .feed + ": " + .outcome] | join("\n"))
  + "\n\nThe complete sanitized report and public identity response are attached to the workflow run."
' target/registry-sync.json > "$RUNNER_TEMP/registry-sync-pr.md"
title='chore: sync registry catalog'
draft_args=()
if jq -e '.identity_repairs | length > 0' target/registry-sync.json > /dev/null; then
  title='chore: review registry identity repairs'
  draft_args=(--draft)
  if [ -n "$pr_number" ] && [ "$(jq -r '.isDraft' <<< "$pr")" = false ]; then
    gh pr ready "$pr_number" --undo
  fi
fi
git config user.name 'bitrouter-automation[bot]'
git config user.email '267229870+bitrouter-automation[bot]@users.noreply.github.com'
git add "${paths[@]}"
git commit -m 'chore: sync registry catalog'
# Recheck the accepted source immediately before the conditional branch write.
if [ "$(git ls-remote origin "refs/heads/$REGISTRY_BASE" | awk '{print $1}')" != "$REGISTRY_BASE_SHA" ]; then
  echo '::error::Registry accepted source changed; rerun with fresh observations.'
  exit 1
fi
git push --force-with-lease="refs/heads/$REGISTRY_BRANCH:$REGISTRY_PROPOSAL_SHA" origin "HEAD:$REGISTRY_BRANCH"
if [ -n "$pr_number" ]; then
  gh pr edit "$pr_number" --title "$title" --body-file "$RUNNER_TEMP/registry-sync-pr.md"
else
  gh pr create --base "$REGISTRY_BASE" --head "$REGISTRY_BRANCH" --title "$title" --body-file "$RUNNER_TEMP/registry-sync-pr.md" "${draft_args[@]}"
fi

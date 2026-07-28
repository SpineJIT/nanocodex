#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 3 ]]; then
    echo "usage: $0 <pull-request-number> <artifact-name> <destination>" >&2
    exit 2
fi

pr=$1
artifact=$2
destination=$3
if [[ ! "$pr" =~ ^[1-9][0-9]*$ ]]; then
    echo "pull request number must be a positive integer" >&2
    exit 2
fi
if [[ ! "$artifact" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "artifact name contains unsupported characters" >&2
    exit 2
fi

repository=${GH_REPO:-gakonst/nanocodex}
workflow=nightly.yml

command -v gh >/dev/null 2>&1 || {
    echo "gh is required to download PR artifacts" >&2
    exit 2
}
command -v jq >/dev/null 2>&1 || {
    echo "jq is required to verify PR artifact provenance" >&2
    exit 2
}

pull_request=$(
    gh pr view "$pr" --repo "$repository" \
        --json headRefOid,state,url
)
state=$(jq -r '.state' <<<"$pull_request")
if [[ "$state" != "OPEN" ]]; then
    echo "pull request #$pr is $state; refusing to install a stale artifact" >&2
    exit 1
fi
head_sha=$(jq -r '.headRefOid' <<<"$pull_request")

runs=$(
    gh run list --repo "$repository" --workflow "$workflow" \
        --event workflow_dispatch --limit 100 \
        --json conclusion,databaseId,displayTitle,status,url
)
run=$(
    jq -c --arg title "PR #$pr artifacts" \
        'map(select(.displayTitle == $title)) | first // empty' <<<"$runs"
)
if [[ -z "$run" ]]; then
    echo "no on-demand artifact run exists for PR #$pr" >&2
    echo "run 'just build-pr-artifacts $pr' and wait for it to succeed" >&2
    exit 1
fi
run_status=$(jq -r '"\(.status)/\(.conclusion // "pending")"' <<<"$run")
if [[ "$run_status" != "completed/success" ]]; then
    run_url=$(jq -r '.url' <<<"$run")
    echo "latest PR #$pr artifact run is $run_status: $run_url" >&2
    exit 1
fi

run_id=$(jq -r '.databaseId' <<<"$run")
run_url=$(jq -r '.url' <<<"$run")
destination_dir=$(dirname "$destination")
mkdir -p "$destination_dir"
temporary_dir=$(mktemp -d "${destination_dir}/.nanocodex-pr-download.XXXXXX")
trap 'rm -rf "$temporary_dir"' EXIT

gh run download "$run_id" --repo "$repository" --name "$artifact" \
    --dir "$temporary_dir"

manifest="$temporary_dir/PR_BUILD.json"
checksum="$temporary_dir/$artifact.sha256"
binary="$temporary_dir/$artifact"
for required in "$manifest" "$checksum" "$binary"; do
    if [[ ! -f "$required" ]]; then
        echo "artifact archive is missing $(basename "$required")" >&2
        exit 1
    fi
done

jq -e \
    --arg repository "$repository" \
    --argjson pr "$pr" \
    --arg sha "$head_sha" \
    --arg artifact "$artifact" \
    --argjson run_id "$run_id" \
    '.repository == $repository and
     .pr == $pr and
     .sha == $sha and
     .artifact == $artifact and
     .run_id == $run_id' \
    "$manifest" >/dev/null || {
    echo "artifact provenance does not match the current PR head" >&2
    exit 1
}

expected=$(
    awk -v artifact="$artifact" '$2 == artifact { print tolower($1) }' \
        "$checksum"
)
if [[ ! "$expected" =~ ^[[:xdigit:]]{64}$ ]]; then
    echo "artifact checksum is missing or invalid" >&2
    exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$binary" | awk '{ print $1 }')
else
    actual=$(shasum -a 256 "$binary" | awk '{ print $1 }')
fi
if [[ "$actual" != "$expected" ]]; then
    echo "checksum mismatch for $artifact: expected $expected, got $actual" >&2
    exit 1
fi

chmod 0755 "$binary"
printf '%s\n' "$actual" > "$temporary_dir/nanocodex.sha256"
mv -f "$binary" "$destination"
mv -f "$temporary_dir/nanocodex.sha256" "$destination.sha256"
mv -f "$manifest" "$destination.pr.json"

printf 'Installed %s from PR #%s at %s (%s).\n' \
    "$destination" "$pr" "$head_sha" "$run_url"

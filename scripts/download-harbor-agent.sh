#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
    echo "usage: $0 <release-tag> <destination>" >&2
    exit 2
fi

release_tag=$1
destination=$2
artifact=nanocodex-x86_64-unknown-linux-musl
release_base="https://github.com/gakonst/nanocodex/releases/download/${release_tag}"
destination_dir=$(dirname "$destination")

mkdir -p "$destination_dir"
temporary_dir=$(mktemp -d "${destination_dir}/.nanocodex-download.XXXXXX")
trap 'rm -rf "$temporary_dir"' EXIT

curl --fail --location --retry 5 --retry-all-errors \
    --output "$temporary_dir/SHA256SUMS" \
    "${release_base}/SHA256SUMS"
curl --fail --location --retry 5 --retry-all-errors \
    --output "$temporary_dir/$artifact" \
    "${release_base}/${artifact}"

expected=$(
    awk -v artifact="$artifact" '$2 == artifact { print $1 }' \
        "$temporary_dir/SHA256SUMS"
)
if [[ ! "$expected" =~ ^[[:xdigit:]]{64}$ ]]; then
    echo "release checksum manifest does not contain $artifact" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary_dir/$artifact" | awk '{ print $1 }')
else
    actual=$(shasum -a 256 "$temporary_dir/$artifact" | awk '{ print $1 }')
fi
if [[ "$actual" != "$expected" ]]; then
    echo "checksum mismatch for $artifact: expected $expected, got $actual" >&2
    exit 1
fi

chmod 0755 "$temporary_dir/$artifact"
mv -f "$temporary_dir/$artifact" "$destination"
printf 'Installed %s from %s (%s)\n' "$destination" "$release_tag" "$actual"

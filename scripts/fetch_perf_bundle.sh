#!/usr/bin/env bash
# Download a CI-built perf bundle and verify its checksums.
#
#   scripts/fetch_perf_bundle.sh              # latest successful run on $BRANCH
#   scripts/fetch_perf_bundle.sh <run-id>     # a specific run
#   DEST=dist/foo scripts/fetch_perf_bundle.sh
#
# Prints the extracted bundle path on the last line.
set -euo pipefail

repo="${REPO:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
branch="${BRANCH:-perf/ab-bundle}"
run_id="${1:-}"

if [[ -z "${run_id}" ]]; then
    run_id="$(gh run list \
        --repo "${repo}" \
        --workflow perf-bundle.yml \
        --branch "${branch}" \
        --status success \
        --limit 1 \
        --json databaseId \
        --jq '.[0].databaseId // empty')"
fi

if [[ -z "${run_id}" ]]; then
    echo "no successful Perf Bundle run found for branch ${branch} in ${repo}" >&2
    echo "push the branch or pass a run id: $(basename "$0") <run-id>" >&2
    exit 1
fi

dest="${DEST:-dist/perf-bundle-${run_id}}"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

echo "downloading artifact for run ${run_id} from ${repo}" >&2
gh run download "${run_id}" --repo "${repo}" --name doggy-dns-perf-bundle --dir "${tmp}"

tarball="$(find "${tmp}" -maxdepth 1 -name '*.tar.gz' -print -quit)"
if [[ -z "${tarball}" ]]; then
    echo "artifact does not contain a .tar.gz bundle" >&2
    exit 1
fi

mkdir -p "${dest}"
tar -xzf "${tarball}" -C "${dest}" --strip-components=1
chmod +x "${dest}"/bin/*

(
    cd "${dest}"
    sha256sum -c SHA256SUMS
) >&2

cat "${dest}/MANIFEST.txt" >&2
echo "bundle: ${dest}" >&2
printf '%s\n' "${dest}"

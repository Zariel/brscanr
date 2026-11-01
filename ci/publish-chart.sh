#!/usr/bin/env bash
set -euo pipefail

image_ref=$(python -c 'import os,json; print(json.loads(os.environ["IMAGE_META_JSON"])["tags"][0])')
python ci/package-chart.py \
  --ref "$GITHUB_REF_NAME" --ref-type "$GITHUB_REF_TYPE" \
  --image-ref "$image_ref" --image-digest "$IMAGE_DIGEST" \
  --destination "$RUNNER_TEMP/charts" > "$RUNNER_TEMP/chart.json"
chart=$(python -c 'import sys,json; print(json.load(open(sys.argv[1]))["file"] or "")' "$RUNNER_TEMP/chart.json")
if [ -z "$chart" ]; then
  echo "Skipping Helm publication: a SemVer Git tag is required."
  exit 0
fi

printf '%s' "$GHCR_TOKEN" | helm registry login ghcr.io --username "$GITHUB_ACTOR" --password-stdin
helm push "$chart" "oci://ghcr.io/${GITHUB_REPOSITORY_OWNER,,}/charts"

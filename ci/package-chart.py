#!/usr/bin/env python3
"""Package a chart with the exact published image, without changing the checkout."""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

import yaml


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ref", required=True)
    parser.add_argument("--ref-type", choices=["branch", "tag"], required=True)
    parser.add_argument("--run-number", type=int, default=0)
    parser.add_argument("--run-attempt", type=int, default=1)
    parser.add_argument("--image-ref", required=True)
    parser.add_argument("--image-digest", required=True)
    parser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    chart = Path("charts/brscanr")
    metadata = yaml.safe_load((chart / "Chart.yaml").read_text())
    # Helm requires SemVer versions; arbitrary Git tags still publish containers.
    number = r"(?:0|[1-9][0-9]*)"
    prerelease = r"(?:0|[1-9][0-9]*|[0-9]*[a-zA-Z-][0-9a-zA-Z-]*)"
    semver = rf"{number}\.{number}\.{number}(?:-{prerelease}(?:\.{prerelease})*)?(?:\+[0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*)?"
    if args.ref_type == "branch" and args.ref == "main":
        base = metadata["version"].split("-")[0].split("+")[0]
        version = f"{base}-main.{args.run_number}.{args.run_attempt}"
        app_version = args.ref
    elif args.ref_type == "tag" and re.fullmatch(rf"v?{semver}", args.ref):
        version = args.ref.removeprefix("v")
        app_version = version
    else:
        print(json.dumps({"file": None}))
        return
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.image_digest):
        parser.error("image digest must identify the published multi-platform manifest")
    repository, separator, tag = args.image_ref.rpartition(":")
    if not separator or not re.fullmatch(r"[a-z0-9./_-]+", repository) or not re.fullmatch(r"[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,127}", tag):
        parser.error("image reference must contain a repository and a valid tag")
    args.destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="brscanr-chart-") as temporary:
        staged = Path(temporary) / "brscanr"
        shutil.copytree(chart, staged)
        values_path = staged / "values.yaml"
        values = yaml.safe_load(values_path.read_text())
        values["image"].update(repository=repository, tag=tag, digest=args.image_digest)
        values_path.write_text(yaml.safe_dump(values, sort_keys=False))
        subprocess.run(["helm", "package", str(staged), "--version", version,
                        "--app-version", app_version, "--destination", str(args.destination)],
                       check=True, capture_output=True, text=True)
    print(json.dumps({"file": str(args.destination / f"brscanr-{version}.tgz"), "version": version}))


if __name__ == "__main__":
    main()

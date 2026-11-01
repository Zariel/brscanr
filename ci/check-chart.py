#!/usr/bin/env python3
"""Verify rendered workloads, single-owner admission and packaged image pins."""
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile

import yaml


CHART = "charts/brscanr"
VALUES = "ci/chart-values.yaml"


def render(overrides=None, chart=CHART):
    with tempfile.TemporaryDirectory() as temporary:
        values = Path(temporary) / "values.yaml"
        values.write_text(yaml.safe_dump(overrides or {}))
        output = subprocess.check_output(["helm", "template", "office", chart,
                                          "-f", VALUES, "-f", str(values)], text=True)
    documents = list(yaml.safe_load_all(output))
    standard = [d for d in documents if d["kind"] != "ServiceMonitor"]
    subprocess.run(["kubeconform", "-strict", "-summary"],
                   input=yaml.safe_dump_all(standard), text=True, check=True)
    return documents


def kind(documents, name):
    return next(d for d in documents if d["kind"] == name)


def main():
    subprocess.run(["helm", "lint", "--strict", CHART, "-f", VALUES], check=True)
    documents = render()
    deployment = kind(documents, "Deployment")
    spec = deployment["spec"]
    assert spec["replicas"] == 1
    assert spec["strategy"] == {"type": "Recreate"}
    pod = spec["template"]["spec"]
    assert pod["automountServiceAccountToken"] is False
    assert pod["securityContext"]["runAsUser"] == 65532
    container = pod["containers"][0]
    assert container["securityContext"]["readOnlyRootFilesystem"] is True
    assert container["readinessProbe"]["httpGet"]["path"] == "/readyz"
    assert container["image"] == "ghcr.io/zariel/brscanr:v0.1.1"
    secret = container["env"][0]["valueFrom"]["secretKeyRef"]
    assert secret == {"name": "brscanr-paperless", "key": "token"}
    assert "BRSCANR_PAPERLESS_TOKEN" not in kind(documents, "ConfigMap")["data"]
    pvc = kind(documents, "PersistentVolumeClaim")
    assert "storageClassName" not in pvc["spec"]
    assert pvc["metadata"]["annotations"]["helm.sh/resource-policy"] == "keep"

    changed = render({"callback": {"port": 54926}, "service": {"port": 80},
                      "persistence": {"storageClass": ""}, "serviceMonitor": {"enabled": True}})
    config = kind(changed, "ConfigMap")["data"]
    assert config["BRSCANR_CALLBACK"] == "198.51.100.50:54926"
    pod = kind(changed, "Deployment")["spec"]["template"]["spec"]
    assert {p["name"]: p["containerPort"] for p in pod["containers"][0]["ports"]} == {"http": 8080, "callback": 54926}
    services = [d for d in changed if d["kind"] == "Service"]
    callback = next(d for d in services if d["spec"]["type"] == "LoadBalancer")
    http = next(d for d in services if d["spec"]["type"] == "ClusterIP")
    assert callback["spec"]["externalTrafficPolicy"] == "Local"
    assert callback["spec"]["ports"] == [{"name": "callback", "port": 54926, "targetPort": "callback", "protocol": "UDP"}]
    assert http["spec"]["ports"][0]["port"] == 80
    assert kind(changed, "PersistentVolumeClaim")["spec"]["storageClassName"] == ""
    assert kind(changed, "ServiceMonitor")["spec"]["endpoints"][0]["port"] == "http"

    existing = render({"replicaCount": 0, "persistence": {"existingClaim": "saved-scans"}})
    assert not any(d["kind"] == "PersistentVolumeClaim" for d in existing)
    deployment = kind(existing, "Deployment")
    assert deployment["spec"]["replicas"] == 0
    assert deployment["spec"]["template"]["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"] == "saved-scans"
    for setting in ["replicaCount=2", "scanner.buttonSource=color", "callback.port=0"]:
        rejected = subprocess.run(["helm", "template", "office", CHART, "-f", VALUES, "--set", setting], capture_output=True)
        assert rejected.returncode != 0, setting

    original = Path(CHART, "values.yaml").read_bytes()
    with tempfile.TemporaryDirectory() as temporary:
        for ref, ref_type, expected in [("main", "branch", None),
                                         ("v1.2.3", "branch", None),
                                         ("v1.2.3", "tag", "1.2.3"),
                                         ("1.2.3", "tag", "1.2.3"),
                                         ("v1.2.3-rc.1", "tag", "1.2.3-rc.1"),
                                         ("v1.2.3+build.1", "tag", "1.2.3+build.1"),
                                         ("v1.02.3", "tag", None),
                                         ("v1.2.3-01", "tag", None),
                                         ("nightly", "tag", None)]:
            output = subprocess.check_output(["python", "ci/package-chart.py", "--ref", ref, "--ref-type", ref_type,
                                              "--image-ref", "ghcr.io/zariel/brscanr:main",
                                              "--image-digest", "sha256:" + "a" * 64, "--destination", temporary], text=True)
            packaged = json.loads(output)
            if expected is None:
                assert packaged["file"] is None
                continue
            assert packaged["version"] == expected
            with tarfile.open(packaged["file"]) as archive:
                values = yaml.safe_load(archive.extractfile("brscanr/values.yaml"))
                metadata = yaml.safe_load(archive.extractfile("brscanr/Chart.yaml"))
                assert metadata["version"] == expected
                assert values["image"]["digest"] == "sha256:" + "a" * 64
            image = kind(render(chart=packaged["file"]), "Deployment")["spec"]["template"]["spec"]["containers"][0]["image"]
            assert image == "ghcr.io/zariel/brscanr@sha256:" + "a" * 64
    assert Path(CHART, "values.yaml").read_bytes() == original
    print("Chart checks passed: workload contracts, storage, source preservation, schema, and digest-pinned release packages.")


if __name__ == "__main__":
    main()

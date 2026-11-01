# brscanr

brscanr receives **Scan to PC** button events from Brother printers, acquires a PDF, and imports it into Paperless-ngx through its API. You can also start a scan from its web interface or HTTP API.

The service runs independently of Paperless. Completed scans remain on its persistent volume during Paperless outages. Restarting brscanr never automatically repeats physical acquisition, and retrying delivery never moves paper.

## Supported scanning

The MFC-L2800DW protocol is supported with two 300 dpi grayscale, single-sided presets: one platen page, or an entire feeder stack combined into one PDF. All four Scan to PC functions use your configured button preset; their labels do not select OCR, email delivery, or a paper source. Paperless handles OCR and classification.

The verified scan areas are approximately 207 × 291 mm for the platen and 210 × 294 mm for the feeder, including device padding. These are fixed presets, not edge-to-edge A4 capture. Check page edges on representative documents before using the service for archival scans. Other printer models, duplex, color, and custom scan geometry are not supported presets.

## Run locally

You need Nix with flakes enabled, network access to the printer, a routable IPv4 callback address, and a Paperless API token with document-upload, document-view, and task-view permissions. Stop any other scanner owner before testing the same destination. Use a separate destination name for evaluation.

Enter the development environment and run the service:

```sh
nix develop
export BRSCANR_PRINTER_IP=192.0.2.10
export BRSCANR_CALLBACK=192.0.2.20:54925
export BRSCANR_DESTINATION=ScanTest
export BRSCANR_INSECURE_PRINTER_TLS=true
export BRSCANR_PAPERLESS_URL=https://paperless.example.com/
export BRSCANR_PAPERLESS_TOKEN_FILE="$PWD/.secrets/paperless-token"
export BRSCANR_SPOOL="$PWD/spool"
cargo run -p brscanr
```

Replace the example addresses and token path. `BRSCANR_INSECURE_PRINTER_TLS=true` accepts the printer's self-signed HTTPS certificate; it affects only printer registration.

Open `http://localhost:8080`, load paper, and select **Scan at 300 dpi**. For a physical-button scan, select **Scan to PC → File → ScanTest** on the printer. The default button source is the feeder; set `BRSCANR_BUTTON_SOURCE=platen` to use the glass. The web interface shows acquisition and delivery separately, retained PDFs, and the Paperless document link after import.

The HTTP interface has no built-in authentication. Restrict it to a trusted network or place it behind an authenticated gateway before exposing it. It grants access to documents and physical scanner controls.

## Deploy in Kubernetes

Container images are published to `ghcr.io/zariel/brscanr` for Linux AMD64 and ARM64. `main` publishes the `main` image tag; Git tags publish their corresponding image tags. The runtime is distroless nonroot with one stripped, statically linked executable and the base image's certificate and identity data.

Install the [Helm chart](charts/brscanr/README.md) from `oci://ghcr.io/zariel/charts/brscanr`. Charts are published only for SemVer Git tags and pin the exact image digest from that release.

For a local build, use the Nix development shell for tooling:

```sh
nix build
nix develop
docker buildx build --load --tag brscanr:local .
```

`nix build` builds the native executable. The Dockerfile uses a separate dependency layer, so source changes reuse compiled dependencies. CI exports all BuildKit layers to an architecture-scoped cache and builds each architecture on a native runner.

Follow the [deployment and recovery guide](docs/operations.md) to reserve a callback address, create the API-token Secret, and verify routed button delivery. You can use Helm or the manifests in [`deploy/kubernetes`](deploy/kubernetes). Both use one replica, a dedicated PVC, normal pod networking, and a UDP LoadBalancer. They require no host networking, Multus attachment, privileged container, or Kubernetes API permissions.

## Trigger scans through HTTP

Use a fresh UUID for each intended physical scan. Reuse it only to retry the same request; keys are retained for seven days.

```sh
curl --fail-with-body http://localhost:8080/api/scans \
  -H 'Content-Type: application/json' \
  -d '{"source":"feeder","idempotency_key":"a507a2ec-6779-437f-b5b1-c789755ffbb3"}'
```

An accepted scan returns `202` and a job. Repeating its key returns that job with `200`; busy, uncertain, or storage-limited admission returns `409`. Rejected keys remain rejected: use a new key after resolving the problem. `GET /api/jobs/{id}` shows the job, `GET /api/jobs` lists the latest 100 jobs, and `GET /api/status` reports registration and callback observations.

## Use the scanning library

The [`brother-scan`](crates/brother-scan) crate provides Tokio acquisition, Phoenix registration, notification parsing, and PDF assembly. It does not depend on the service, SQLite, Kubernetes, or Paperless. See the [library example](crates/brother-scan/examples/scan.rs) and [protocol reference](docs/protocol.md). Run the example only when you intend to operate the physical scanner:

```sh
mkdir -p spool
cargo run -p brother-scan --example scan -- 192.0.2.10 platen spool/example
```

## Develop and verify

Inside `nix develop`, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p brscanr
python tests/workflow.py
python ci/check-chart.py
actionlint
hadolint Dockerfile
kubectl kustomize deploy/kubernetes | kubeconform -strict -summary
```

The workflow test uses loopback simulators and a temporary spool. It exercises the real service process and independently renders its PDF with Poppler. To check a built container, run `BRSCANR_TEST_IMAGE=brscanr:local python ci/check-image.py`; this inspects its filesystem, starts it with a read-only root and persistent volume, and runs the full workflow against the executable extracted from that image. Neither test contacts a physical printer or a real Paperless instance. Physical image quality, cross-subnet callback delivery, and cluster failover require the checks in the deployment guide.

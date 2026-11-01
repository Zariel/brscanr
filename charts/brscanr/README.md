# brscanr Helm chart

Deploys one Brother scanner owner with a persistent scan spool and Paperless-ngx API delivery. The chart is published as a Helm OCI artifact at `oci://ghcr.io/zariel/charts/brscanr`. It requires Kubernetes 1.25 or later, persistent storage, and a UDP LoadBalancer reachable from the printer.

## Install

Reserve a routed callback IPv4 address and create a Paperless API token Secret in the release namespace. The token needs document-upload, document-view, and task-view permissions:

```sh
kubectl create namespace brscanr
kubectl -n brscanr create secret generic brscanr-paperless \
  --from-file=token=.secrets/paperless-token
```

Create `values.yaml`, replacing the documentation addresses and Paperless URL:

```yaml
scanner:
  printerIP: 192.0.2.10
  destination: Paperless
  buttonSource: feeder
  insecureTLS: true
callback:
  address: 198.51.100.50
  annotations:
    lbipam.cilium.io/ips: 198.51.100.50
paperless:
  url: http://paperless.default.svc.cluster.local:8000/
  existingSecret: brscanr-paperless
```

The annotation selects a reserved address with Cilium. For another LoadBalancer controller, use its address-selection mechanism. `scanner.insecureTLS` explicitly accepts the printer's self-signed certificate; it does not affect Paperless TLS.

Inspect and install a chart version:

```sh
helm show chart oci://ghcr.io/zariel/charts/brscanr --devel
helm upgrade --install brscanr oci://ghcr.io/zariel/charts/brscanr \
  --namespace brscanr --version <chart-version> --values values.yaml
kubectl -n brscanr port-forward service/brscanr 8080:8080
```

Open `http://localhost:8080` and check registration and real button delivery. Restrict shared UI access to a trusted network or authenticated gateway. See the [operations guide](../../docs/operations.md) for routing, recovery, and cutover checks.

## Release versions

`main` publishes development charts as `0.1.0-main.<run>.<attempt>` using the base version in `Chart.yaml`. Select them with `--devel` or an exact version. SemVer Git tags such as `v1.2.3` publish chart version `1.2.3`; prerelease and build metadata are supported. Non-SemVer Git tags publish the container only. All published charts pin the exact multi-platform image digest from the same workflow, including development charts.

## Configure the deployment

The complete defaults are in [values.yaml](values.yaml). These settings control the deployment:

| Values | Purpose |
| --- | --- |
| `scanner.*` | Printer IPv4 address, destination name, button source, and printer TLS policy |
| `callback.address`, `callback.port`, `callback.annotations` | Advertised callback and UDP LoadBalancer; the default port is 54925 |
| `paperless.url`, `paperless.existingSecret`, `paperless.tokenKey` | Paperless API base URL and existing token Secret/key |
| `image.repository`, `image.tag`, `image.digest` | Container selection; a digest overrides the tag |
| `replicaCount` | `1` runs the scanner; `0` stops it; larger counts are rejected |
| `persistence.existingClaim` | Reuse a PVC instead of creating one |
| `persistence.size`, `persistence.storageClass`, `persistence.accessModes` | New PVC settings; defaults to 10 GiB, cluster-default storage, and ReadWriteOnce |
| `persistence.annotations` | Defaults to retaining the PVC on uninstall |
| `extraEnv`, `envFrom` | Additional native application settings; keep credentials in Secret references |
| `service.port`, `service.annotations` | HTTP ClusterIP Service |
| `serviceMonitor.*` | Optional Prometheus Operator discovery; requires its ServiceMonitor CRD |
| `resources`, `nodeSelector`, `tolerations`, `affinity` | Resource and scheduling settings |
| `podAnnotations`, `imagePullSecrets` | Pod annotations and registry credentials |
| `podSecurityContext`, `securityContext` | Default distroless UID/GID 65532, read-only root, dropped capabilities, and no privilege escalation |
| `nameOverride`, `fullnameOverride` | Resource naming overrides |
| `terminationGracePeriodSeconds` | Shutdown grace period; minimum 25 seconds |

Use `extraEnv` for limits and retention, for example:

```yaml
extraEnv:
  - name: BRSCANR_MAX_PAGES
    value: "50"
  - name: BRSCANR_RETENTION_DAYS
    value: "14"
```

The chart owns the HTTP bind address, spool path, callback address/port, and Paperless token configuration. Avoid overriding these through `extraEnv` or `envFrom`, since they must agree with the Services and volume mounts. Changing chart configuration restarts the pod. Restart the Deployment after changing the existing Secret's token.

The Deployment uses `Recreate` and always mounts a PVC. These prevent ordinary overlap and document loss during rollouts, but do not provide distributed fencing. Stop or fence an isolated old owner before force-replacing its pod. Keep the PVC until outstanding scans are reconciled; uninstall leaves it in place by default.

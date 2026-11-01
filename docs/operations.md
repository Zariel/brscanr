# Deploy and operate brscanr

## Prepare networking

Give the printer a stable IPv4 address. Reserve an unused LoadBalancer address routed from the printer's network. On Cilium, reserve it in the configured LoadBalancer pool and BGP advertisements; set the `lbipam.cilium.io/ips` annotation on `brscanr-callback`. With another LoadBalancer controller, use its address-reservation mechanism instead.

Set `BRSCANR_CALLBACK` to that address and UDP port 54925. Use `externalTrafficPolicy: Local` so the callback retains the printer's source address: brscanr rejects other source addresses. The UDP listener binds before registration. Readiness does not wait for a callback, so Service publication can establish the route.

Allow these flows and their return traffic:

| Initiator | Destination | Traffic |
| --- | --- | --- |
| brscanr pod | Printer | TCP 443 for registration, TCP 54921 for acquisition |
| Printer | Callback LoadBalancer address | UDP 54925, arbitrary source port |
| brscanr pod | Paperless | HTTP(S) upload and task status, typically TCP 8000 |
| brscanr pod | Cluster DNS | UDP and TCP 53 |
| Authenticated gateway or trusted clients | brscanr HTTP Service | TCP 8080 |

The callback is a new inbound flow, not a reply to registration. Outbound printer connections use the pod's routed or masqueraded source address, not necessarily the callback VIP. Ensure return routing to that address. With routed pod networking, this includes the pod CIDRs. A VIP in the printer's directly connected subnet requires a working Layer 2 advertisement; a routed VIP normally belongs to a different subnet.

The optional [`network-policy.example.yaml`](../deploy/kubernetes/network-policy.example.yaml) is excluded from the base deployment. Adjust its printer IP, DNS namespace, Paperless pod labels, and client/gateway rules before applying it. Verify source preservation and policy behavior in your CNI datapath. The sample allows HTTP from cluster namespaces; narrow it for your gateway.

## Install the service

1. Build, load, and push the Nix image described in the README. Set a registry image reference, preferably a digest, in the Deployment.
2. Edit `deploy/kubernetes/config.yaml`: printer IP, callback VIP, destination, Paperless URL, and button source. Update the callback Service annotation to match. Choose a PVC storage class that supports SQLite locking and durable file/directory synchronization. The default claim requests 10 GiB.
3. Create the namespace and a Secret containing a file named `token`:

   ```sh
   kubectl apply -f deploy/kubernetes/namespace.yaml
   kubectl -n brscanr create secret generic brscanr-paperless \
     --from-file=token=.secrets/paperless-token
   ```

4. Apply the configured resources:

   ```sh
   kubectl apply -k deploy/kubernetes
   kubectl -n brscanr rollout status deployment/brscanr
   kubectl -n brscanr get service brscanr-callback
   kubectl -n brscanr port-forward service/brscanr 8080:8080
   ```

5. Open `http://localhost:8080`. Check registration acceptance, then press the physical Scan to PC button. Confirm a new **last validated callback** time, a complete PDF with the expected pages, and an imported Paperless document. Inspect the actual Paperless document, including page order and edges.

Use an authenticated gateway for shared HTTP access. The supplied ClusterIP does not publish a public UI. Set a browser-accessible Paperless base URL if you want its document links to work outside the cluster; the same URL must be reachable from the pod.

Never run multiple owners for one printer. `Recreate` and the spool file lock prevent ordinary local overlap; they are not distributed fencing. After a node partition, stop or fence the old node before force-replacing the pod. A file lock cannot protect two independent PVCs or an old process that still has printer access.

## Configuration

Configuration comes from environment variables. Restart the service after changing configuration or the Paperless token.

| Variable | Default | Purpose |
| --- | --- | --- |
| `BRSCANR_PRINTER_IP` | Required | Printer IPv4 address |
| `BRSCANR_CALLBACK` | Required | Advertised IPv4 address and UDP port |
| `BRSCANR_PAPERLESS_URL` | Required | HTTP(S) base URL ending in `/`; subpaths are supported |
| `BRSCANR_PAPERLESS_TOKEN_FILE` | None | Token file; takes precedence over the token variable |
| `BRSCANR_PAPERLESS_TOKEN` | Required without token file | Paperless API token |
| `BRSCANR_DESTINATION` | `Paperless` | 1–32 ASCII letters, digits, spaces, underscores or hyphens |
| `BRSCANR_BUTTON_SOURCE` | `feeder` | `feeder` or `platen`, independent of notification source fields |
| `BRSCANR_INSECURE_PRINTER_TLS` | `false` | Accept the printer's self-signed TLS certificate |
| `BRSCANR_LISTEN` | `0.0.0.0:8080` | HTTP bind address |
| `BRSCANR_SPOOL` | `/var/lib/brscanr` | Persistent database and document directory |
| `BRSCANR_MAX_PAGES` | `100` | Per-scan page limit, 1–1000 |
| `BRSCANR_MAX_SCAN_BYTES` | `268435456` | Maximum compressed image bytes per scan |
| `BRSCANR_MIN_FREE_BYTES` | `1073741824` | Free-space floor for admission |
| `BRSCANR_MAX_SPOOL_BYTES` | `8589934592` | Spool admission threshold |
| `BRSCANR_RETENTION_DAYS` | `7` | Retain imported files for this many days |
| `RUST_LOG` | `brscanr=info` | Structured JSON log filter |

Admission reserves three scan-byte budgets for raw data, a PDF, and concurrent assembly, in addition to the free-space floor. It rejects new work before exhausting the configured spool budget. Other writers or filesystem failures can still exhaust the disk; incomplete scans remain incomplete. Never delete unimported files to make room automatically. Imported files alone are removed by retention; job metadata remains in the ledger. Size the container memory limit for PDF assembly if you increase the scan-byte limit.

## Recover jobs

| State | Meaning and next action |
| --- | --- |
| `acquiring` | Physical acquisition is active; new scans are rejected |
| `captured` | Complete pages and job-end evidence are retained; PDF assembly runs locally |
| `ready` | Complete PDF is queued; known pre-submission failures retry with backoff |
| `uploading` | Request is crossing the Paperless acceptance boundary |
| `submitted` | A task UUID is durable; only task polling is retried |
| `imported` | Paperless confirmed a document ID |
| `interrupted` | Acquisition failed or the service restarted mid-scan; inspect printer before acknowledging |
| `unknown` | Upload acceptance or task outcome is uncertain; check Paperless before confirming delivery retry |
| `delivery_failed` | Upload was rejected or processing failed; resolve the cause and retry delivery |
| `failed` | Local artifact or PDF assembly failed; raw files remain for inspection |
| `deleted` | Imported local files have passed retention and were removed |

After interruption, use **I checked the printer; allow scans** only after clearing any jam and confirming that the printer is idle. Starting a new scan is always an explicit action. Acquisition has a 10-second connect deadline, a 30-second read-idle deadline, and a 15-minute overall deadline; closing its connection is not a verified printer cancellation command.

For `unknown`, search Paperless for the uploaded filename `brscanr-<job UUID>.pdf` and inspect tasks/documents. If you find the matching document, enter its document ID and select **Record existing import**. Confirm that its pages match this scan; the service checks that the document exists, records the resolved import, and sends no upload. A retained SHA256 helps identify the local artifact; it is not a server idempotency key. If the document was not imported, **Retry delivery** requires acknowledgment of duplicate risk. The service does not promise exactly-once import. A failed Paperless task can also have side effects; inspect it before retrying.

The spool contains `jobs.sqlite` with its WAL files, an `owner.lock`, and per-job UUID directories. Complete pages are JPEGs, partial pages use `.part`, and complete capture evidence is `capture.json`. A PDF is published as `document.pdf`. Back up the whole volume consistently while the service is stopped or using a storage snapshot that includes the database and artifacts. Preserve orphaned directories for inspection; they are never silently imported. Missing referenced artifacts become visible errors. Do not edit the database while the service is running.

Button identities include sender, callback, destination, function, registration ID, and sequence. They are retained for 24 hours across restarts, including busy rejections. Identifier reuse beyond that window is treated as a new event. Printer identity reuse is not globally specified; if a reboot reuses the same identifiers within the window, a legitimate event can be suppressed. Remote scan keys are retained for seven days. Use the UI with a new key if an event is suppressed; do not automatically clear deduplication history.

## Monitor and verify rollout

`/livez` reports HTTP responsiveness. `/readyz` checks initialized service state, the database, and spool accessibility. Printer and Paperless outages do not make the service unready or cause liveness restarts. `/metrics` exposes Prometheus process metrics, including open descriptors, registration failures/expiry, last callback, capture successes/failures, undelivered count and age, and spool bytes. Registration requests use a single reusable HTTP client with bounded responses and timeouts; successful leases renew roughly every two minutes with backoff after failures.

Before replacing an existing scanner deployment, verify these environment-specific behaviors:

- A real platen page and a three-page feeder stack produce readable PDFs through both HTTP and the physical button, with the intended page edges and order.
- The routed callback arrives from the printer IP after ordinary pod rescheduling. Confirm the network path independently with CNI flow observations or packet capture.
- Printer restart/network loss recovers registration. Paperless downtime retains scans and later imports them. Service restart preserves queued scans without moving paper again.
- The callback VIP works when registration originates from a different pod address. Registration acceptance alone cannot prove this.

Use a distinct evaluation destination while testing. For cutover, stop the old owner and allow its lease to expire, then use the final destination name. Remove the old scanner sidecar and any trusted-LAN attachment only after the routed workflow is proven. To roll back, stop brscanr before restoring the old owner; retain its PVC until every outstanding job is reconciled. The repository's loopback checks do not prove these physical and cluster-specific conditions.

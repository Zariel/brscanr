# Brother scanner protocol

This reference describes the observed MFC-L2800DW protocol implemented by `brother-scan`. It is not a capability specification for all Brother models. All network operations are unicast; discovery, SNMP registration, speculative UDP acknowledgments, and unverified abort commands are unnecessary for this workflow.

## Register a destination

POST JSON to `https://<printer>/phoenix/mib` using HTTP Digest authentication with the public driver identity `Public:0000`. This is a protocol identity, not the printer administrator password. Device certificates can be self-signed; certificate bypass must be selected explicitly.

Each item in `request` uses key `1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0` and a `string_value` of this form:

```text
TYPE=BR;BUTTON=SCAN;USER=Office;FUNC=FILE;HOST=198.51.100.50:54925;APPNUM=5;DURATION=360;BRID=;
```

Register IMAGE/1, OCR/3, EMAIL/2, and FILE/5. Require four results in `response`, each with `status:0`. Requesting a lease does not establish callback reachability. The service requests 360 seconds and renews after 115–125 seconds; failure retries back off to 60 seconds. It reacquires registration on startup and lets the lease expire after shutdown.

## Receive button events

Events arrive at the advertised UDP address, usually port 54925. Validate sender IP independently of payload identity. The observed header starts with `02 00`, has a variable third byte, and ends with `30`. Following bytes are semicolon-separated ASCII fields:

```text
TYPE=BR;BUTTON=SCAN;USER="Office";FUNC=FILE;HOST=198.51.100.50:54925;APPNUM=5;P1=0;P2=0;P3=0;P4=0;REGID=12345;SEQ=1;
```

Require the configured name and callback, a supported function, and nonempty numeric `REGID` and `SEQ`. Reject malformed, oversized, duplicate-key, or incorrectly addressed messages. Source fields P1–P4 do not reliably distinguish platen and feeder. Retransmissions occur; the service durably consumes identities before operating the scanner, including rejections while busy. No application UDP acknowledgment is required by the observed successful workflow.

## Acquire pages

Connect to TCP 54921. Require the nine-byte greeting `+OK 200\r\n`; `-NG 401\r\n` indicates rejected session admission and must not trigger automatic physical retries.

Commands begin with ESC (`1b`), contain newline-separated ASCII, and end with `80`. Send `SSP` settings, require `00 53 53 50` (`\0SSP`), read a little-endian 32-bit payload length and that many bytes, then send `XSC`. Both commands select 300 × 300 dpi. The complete settings are in `acquisition.rs`; they request GRAY256 JPEG, normal processing, single-sided capture, and disable optional image transformations.

| Source | SSP fields | XSC area |
| --- | --- | --- |
| Platen | `PSRC=FB`, `PAGE=1` | `0,0,2445,3435` |
| Feeder | `PSRC=ADF`, `PAGE=0` | `36,0,2514,3471` |

These scan-area coordinates are the verified device values, not a general paper-size conversion. Feeder widths can include padding; preserve it rather than silently cropping document edges. Platen `PAGE=1` is necessary for the verified physical-button completion path. Feeder `PAGE=0` ends naturally after the stack empties.

Read records independently of TCP packet boundaries:

| Prefix | Following bytes | Meaning |
| --- | --- | --- |
| `00 02` | page:u16le, format:u16le, length:u32le, lines:u32le, payload | JPEG image fragment; format is `0011` |
| `00 21` | page:u16le | Page end |
| `00 20` | None | Job end |
| `90 00` | None | Empty feeder |
| `91 00` | None | Document jam |

Page numbers start at one and increase after each page end. Image fragments belong to the expected page; the line count is cumulative. Require explicit page ends and job end before declaring a capture complete. Unknown records, limits, truncation, jams, and timeouts produce incomplete jobs. An empty-feeder response is not permission to submit partial output. Never infer completion from a JPEG end marker or TCP disconnect.

## Assemble a document

The final image record's `lines` value gives the acquired page height. Feeder JPEG SOF headers can overstate that height. Validate the reported height against the header, replace only the SOF height, and decode the result before embedding the JPEG directly into a PDF at 300 dpi. The compressed samples and width padding remain intact. The printer can emit three-component JPEGs despite the GRAY256 request; select DeviceGray or DeviceRGB from the actual decoded image metadata. Require bounded dimensions and an end-of-image marker.

The library preserves complete raw pages and the current partial file on acquisition failure. The service durably publishes complete capture metadata before local PDF assembly. Assembly can repeat without a new printer connection. PDFs are eligible for upload only after validation and durable publication.

## Deliver to Paperless

POST the PDF as multipart field `document` to `/api/documents/post_document/`, store the returned task UUID, and poll `/api/tasks/?task_id=<uuid>`. The client requests API v9, whose task list exposes Celery-style status and `related_document`. Only a successful task with a document ID establishes import. See the [Paperless upload contract](https://docs.paperless-ngx.com/api/#file-uploads) and [v3.4 task serializer](https://github.com/paperless-ngx/paperless-ngx/blob/v3.4.0/src/documents/serialisers.py).

Persist `uploading` before sending. A failed connection establishment or explicit rate-limit rejection can retry with backoff. A connection failure after transmission, unreadable task UUID, interrupted upload, or disappeared task record requires reconciliation. Task polling never repeats the upload. HTTP acceptance alone does not prove document import.

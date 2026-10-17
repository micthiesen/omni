# Protocol

The wire contract between Slate and Omni. Code: `firmware/protocol`
(`slate-protocol`), which the server will depend on by path so both ends share the types.
Keep this document and that crate in step.

## Transport

CoAP (RFC 7252) over UDP over Thread, server port 5683. Confirmable requests. Frames use
Block2 (RFC 7959), 512-byte blocks provisionally (`SPIKE-TRANSFER-ENERGY` compares 256,
512 and 1024). All integers in payloads are little-endian.

Security: Thread link-layer encryption on the mesh. The server accepts requests only from
the mesh prefix and only for provisioned device ids. No DTLS in Rev A.

## Resources

### `POST s1/check`

Request body (`CheckRequest`):

| Field | Type | Meaning |
| --- | --- | --- |
| `shown_version` | u32 | Version on the panel; 0 if none since provisioning |
| `battery_mv` | u16 | Cell voltage |
| `flags` | u8 | bit 0 USB present, bit 1 charging |
| `refreshes_since_boot` | u32 | Waveforms run since reset |
| `reset_reason` | u8 | `ResetReason` |
| `firmware` | u16 | Build number |
| `parent_rssi_dbm` | i8 | Last frame from the Thread parent |

Response body (`CheckResponse`), first byte is the kind:

- `0` **NoChange**: `next_check_s: u16`.
- `1` **NewFrame**: `version: u32`, `encoded_len: u32`, `crc32: u32`,
  `waveform: u8` (0 full, 1 fast), `next_check_s: u16`.

The device identifies itself with the request's source address and a `d=<id>` URI query
holding its provisioned id.

### `GET s1/frame/<version>`

Block2 transfer of the encoded frame. The server keeps the most recent two versions so a
fetch that races a new publish still completes.

## Frame encoding

```text
"SLT1" | version u32 | rows u16 (680) | row_bytes u16 (240) | 680 row records
row record = tag u8, then:
  0 Raw            240 literal bytes
  1 PackBits       runs until exactly 240 bytes decode
  2 RepeatPrevious nothing (invalid on row 0)
```

Pixels are two bits, packed in the controller's native order and ink codes (provisional
until gate G-01; `slate_protocol::frame::Ink`). The CRC-32/ISO-HDLC in the check response
covers the 163,200 decoded bytes in stream order. The device starts a waveform only after
the CRC matches.

The server picks the smallest record per row. Dashboard frames are mostly white with long
runs, so typical frames are expected well under the 163 kB raw size; the study's energy
model assumes about 50 kB.

## Server refresh policy

The device shows whatever version the server publishes, so the server owns the battery
budget. Rules for the renderer:

- Publish a new version only when the rendered pixels differ from the last published
  frame (compare the encoded bytes, not the source data).
- Coalesce: after a change, wait a short settling window before publishing so bursts of
  updates become one refresh.
- Quiet hours: no new versions overnight; check interval stretched to the end of quiet
  hours.
- Daily cap on published versions, with priority for content the owner marks urgent.
- Every frame renders an "updated at" time and the battery band from the latest check.
- Choose the check interval: 60 s while changes are possible, longer when the next change
  is known to be far away.

Numbers for the window, quiet hours and cap are set when the dashboard content is
designed; [power-budget.md](power-budget.md) gives the energy cost of each refresh.

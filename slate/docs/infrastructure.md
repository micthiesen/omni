# Infrastructure

What Slate needs outside the device. None of it exists yet.

## Thread border router on boris

An OpenThread Border Router (the `openthread/otbr` container) on boris, with a USB 802.15.4
radio co-processor (RCP) running OpenThread RCP firmware (decision D-05).

- Forms its own Thread network on a channel apart from the Apple Home Thread network.
- Owns the active dataset; the operational dataset is exported once with `ot-ctl dataset
  active -x` and written to each Slate over USB (`provision dataset`). The dataset is a
  secret: it lives in the OTBR volume and in a private local file, never in git.
- Advertises an off-mesh-routable prefix and installs the host route to the mesh, so
  processes on boris reach Slate addresses directly.
- RCP candidates are in [`bom/bom.csv`](../bom/bom.csv). Place the dongle on a USB
  extension away from the host's USB 3 ports, which emit 2.4 GHz noise.

Compose service, volume and device mapping are added with the boris deployment work under
`/home/michael/compose`; record the details there and link them here.

## Omni side

A future `omni-slate` subsystem crate in this repository, following the crate conventions
in the root `AGENTS.md`:

- **CoAP endpoint:** UDP 5683, reachable from the mesh. The container needs IPv6 to the
  mesh prefix; host networking for that one port is the simplest route, otherwise a Docker
  IPv6 network with the OTBR route. Settle this with the OTBR deployment.
- **Renderer:** produces 960 x 680 four-ink frames from dashboard content (calendar,
  deliveries, live streamers, weather or whatever the dashboard design settles on), in the
  panel's native packing, encoded with `slate-protocol`'s row codec.
- **Refresh policy:** coalescing, quiet hours, daily cap ([protocol.md](protocol.md)).
- **Telemetry:** each check stored with battery mV, RSSI and reset reason. A Slate silent
  for longer than its expected interval, or reporting a low band, raises one notification
  through the normal notification path.
- **UI:** a page showing the current frame image, last check, battery trend and refresh
  count per day.
- **Side effects:** the endpoint answers requests and sends nothing unsolicited, so
  `SideEffectMode` needs no special handling beyond not publishing frames in preview runs.

When the crate lands, its path dependency on `slate/firmware/protocol` must enter the
image build and server CI: add `slate/firmware/protocol/` to the `.dockerignore`
allowlist and stop `ci.yml` ignoring `slate/firmware/protocol/**` (or move the protocol
crate under `crates/`).

The device protocol is the stable boundary; the renderer and policy can change without a
firmware update as long as `slate-protocol` stays compatible.

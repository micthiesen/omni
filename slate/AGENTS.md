# Omni Slate

Battery-powered 13.3-inch four-ink e-paper display for Omni: ESP32-H2 on Thread,
server-side rendering, a load-switched SSD2677 panel and a protected 1S pouch. This
directory owns its requirements, parts, firmware, PCB source and test evidence; the server
side will be an `omni-slate` crate in the parent workspace.

**Read [docs/STATE.md](docs/STATE.md) first.** Decide an open next step with
`$slate-next`; end a session with `$slate-wrap`.

## Map

- `docs/`: [overview](docs/overview.md), [decisions](docs/decisions.md) (baseline,
  departures from the design study, gates), [electrical](docs/electrical.md),
  [firmware](docs/firmware.md), [protocol](docs/protocol.md),
  [infrastructure](docs/infrastructure.md), [power budget](docs/power-budget.md),
  [mechanical](docs/mechanical.md), [build](docs/build.md), [sources](docs/sources.md).
- `bom/bom.csv`: selections with design and purchase status.
- `testing/test-matrix.csv`: acceptance checks and their evidence.
- `firmware/`: `protocol/` (wire contract shared with the server), `core/` (sans-I/O
  behaviour), `app/` (separate ESP32-H2 workspace).
- `pcb/`: tscircuit mainboard source.

## Design invariants

- The panel refresh is the battery. Never start a waveform on a frame whose length and
  CRC have not been verified, and keep refresh decisions (coalescing, quiet hours, daily
  cap) on the server.
- The device renders nothing. All layout, text and imagery are server-side; the device
  streams rows and holds two row buffers, never a frame.
- Panel power is removed between updates: controller sleep command, every panel line
  driven low, then the load switch off. No GPIO may back-power the unpowered panel.
- While USB power is present the device never sleeps.
- No OTA (owner decision for battery life); updates go over USB-C.
- Thread SED on the Omni-owned OTBR; no Matter, Wi-Fi or BLE commissioning.
- Numbers marked provisional are replaced by measurements; update the doc and the test
  matrix row together.

## Firmware conventions

Follow `../../stillair`: stable Rust, `riscv32imac-unknown-none-elf`, esp-hal with
esp-rtos/Embassy, no ESP-IDF or other C SDK builds. The one C component is OpenThread,
used only through the esp-rs `openthread` crate's prebuilt libraries; enabling a
non-default OpenThread feature would require a CMake/Clang build, so don't. Behaviour
goes in `slate-core` with a test and injected time; `app/` stays thin. Integer units
(millivolts, milliseconds), enums for states, small modules, no debug leftovers. Keep
`slate-protocol` dependency-free and in step with `docs/protocol.md`; the server will
depend on it by path. Flash with `cargo run --release` in `firmware/app` (plain espflash
is correct without OTA). Do not flash hardware unless asked.

## PCB

Tooling and authority follow crystal-shim (see [pcb/README.md](pcb/README.md)). Port its
`$pcb`, `$konnect` and `$kicad-manufacture` skills before capture. Never text-edit
`.kicad_*` files. Passing source checks is not an electrical review.

## Gate and delivery

`sh scripts/check.sh` runs host fmt/clippy/tests, the H2 release build, and PCB
lint/format/typecheck/source checks; CI runs the same in `.github/workflows/slate.yml`.
Completed work ships directly to `main` like the rest of this repository. Pushes that only
touch `slate/` do not trigger the Omni image build or deploy.

This repository is public. Thread datasets, network keys and other credentials, and
personal details such as addresses, never enter git.

Keep durable knowledge in these files, not personal memory; STATE.md holds pointers and
decisions. Update this file when a convention changes.

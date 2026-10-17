# Firmware

The device behaviour contract and the firmware stack. Status: **scaffold**. The crates
build and carry the contract types; no behaviour is implemented yet.

## Layout

| Path | Role |
| --- | --- |
| `firmware/protocol` | `slate-protocol`: the wire contract shared with Omni. No dependencies; builds on the host and the H2. |
| `firmware/core` | `slate-core`: device behaviour as sans-I/O logic. Takes sampled inputs, network outcomes and an injected `Millis`; returns `Action`s. No esp-* dependency. |
| `firmware/app` | The ESP32-H2 binary, its own workspace and target. Thin wiring only. |

New behaviour goes in `slate-core` with a test. `app/` stays thin enough to need none.

## Stack

Bare-metal Rust on stable, target `riscv32imac-unknown-none-elf`: esp-hal, esp-rtos with
Embassy, esp-radio's 802.15.4 driver, and the esp-rs `openthread` crate in MTD/SED mode
with its prebuilt libraries (D-03). `tinyrlibc` supplies the `str*` symbols OpenThread
imports. Logging through `log` and esp-println on USB Serial/JTAG; panics through
esp-backtrace. No ESP-IDF.

Stock bootloader and a single factory app partition (D-04). Flash with `cargo run
--release` in `firmware/app`, which runs `espflash flash --chip esp32h2 --monitor`.

Persistent state (Thread dataset, server address, device id, last shown version) is one
small record in a dedicated flash sector, written only on provisioning and after a
successful refresh. esp-storage keeps its `critical-section` feature so no interrupt
handler runs from flash during an erase.

## Update cycle

1. Light-sleep until the server-chosen check interval expires (default 60 s, never below
   30 s).
2. `POST s1/check` with the shown version and telemetry ([protocol.md](protocol.md)).
3. `NoChange`: sleep again for the interval in the response.
4. `NewFrame`: enable the panel rail, reset and initialise the SSD2677, switch Thread to
   rx-on-when-idle, and fetch `s1/frame/<version>` block by block.
5. Decode each row into a 240-byte buffer and write it to panel RAM. Track the CRC-32.
6. If the header, row count, length or CRC is wrong, or the transfer times out: power the
   panel down without a waveform, keep the old version, and report on the next check.
7. Start the requested waveform. Light-sleep with BUSY as the wake source; a waveform that
   exceeds 40 s is a fault.
8. Controller deep-sleep command, drive every panel line low, open the load switch, return
   to SED polling, store the new shown version.

## Modes

| Mode | Entered when | Behaviour |
| --- | --- | --- |
| Unprovisioned | No stored dataset or server | Stay awake on USB; otherwise deep sleep with USB_PRESENT as the wake source |
| Attaching | Boot, or the parent is lost | Join with the stored dataset; backoff as Offline on failure |
| Idle | Attached | SED polling, light sleep between checks |
| Offline | A check or attach fails | Retry from 60 s doubling to 30 min in light sleep; after 6 h drop Thread and deep-sleep with hourly reattach |
| USB powered | VBUS present | Never sleep; console active; normal cycle continues on the same schedule |
| Battery low | Below 3.60 V | Normal, and reported so the server can render a badge |
| Battery critical | Below 3.45 V | No more waveforms; checks continue at the server interval to report |
| Exhausted | Below 3.30 V | Radio off, deep sleep with USB_PRESENT as the wake source |

Voltage bands are provisional (`slate-core::battery`) until checked against the chosen
pouch's discharge curve. They use 50 mV hysteresis.

## USB console

Line protocol on USB Serial/JTAG, available only while USB power is present (the device
stays awake then). Planned commands:

- `provision dataset <hex TLVs>`, `provision server <ipv6>`, `provision id <name>`
- `status`: mode, Thread role and parent, shown version, battery mV, last error
- `check`: run one check now
- `panel test <pattern>`: full-colour test patterns for the panel gates, no network
- `sleep-test <light|deep> <seconds>`: for the energy spikes

Secrets never appear in console output; the dataset is write-only.

## Logging

Info-level by default (`ESP_LOG`). Log lines cost energy only while USB is attached,
because USB Serial/JTAG is the only log sink; on battery the output goes nowhere.

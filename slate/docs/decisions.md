# Decisions

The starting point was an AI-generated electronic design study (2026-10). This file
records what Slate adopts, where it departs from that study, and the gates that must clear
before each dependent step. Numbers from datasheets are listed in [sources.md](sources.md);
numbers marked provisional are replaced by measurements as they land, and the test matrix
row is updated with them.

## Locked baseline

### D-01 Display: Good Display GDEY133F91

13.3-inch, 960 x 680, four-ink Spectra 3100, SSD2677 on the panel, SPI, 24-pin 0.5 mm FPC.
No true partial refresh is documented, so every visible update is a global waveform. The
design budgets 2.70 J per full update at the panel input (108 mW x 25 s).

**Gate G-01 (before capture):** obtain the current specification and confirm, from it and
a DESPI-C02 bench panel: FPC pinout, whether the panel uses one or two chip selects (large
Spectra panels often split the controller into two halves), BUSY polarity, RAM scan
direction (960 or 680 pixels per row), and the native two-bit ink codes (`PANEL-PINOUT`). Confirm stock
before freezing the mechanical design; the checked listing showed none on hand.

### D-02 MCU: ESP32-H2-MINI-1-H4S

The current 4 MB H4S ordering code, not an older N4 code. The module carries the antenna,
matching and 32 MHz crystal. ESP32-H21 has much lower radio current and is the candidate
for a later revision; Rev A takes the mature certified module.

### D-03 Firmware: bare-metal Rust with the esp-rs `openthread` crate

**Departs from the study**, which proposed ESP-IDF with OpenThread under Rust FFI. Slate
follows the stack of the other firmware projects (stillair, crystal-shim): esp-hal,
esp-rtos/Embassy, no ESP-IDF. The `openthread` crate (0.4) ships prebuilt OpenThread
libraries for `riscv32imac-unknown-none-elf`, runs on esp-radio's 802.15.4 driver, and
supports MTD/SED operation, poll period and link mode control. That keeps the build to
`cargo` alone.

The prebuilt libraries cover the crate's default feature set only. Enabling OpenThread's
own CoAP or other non-default features would force a CMake/Clang build, so Slate speaks
CoAP from Rust over the crate's plain UDP sockets (D-08).

**Gate G-02 (before relying on the light-sleep budget):** on an ESP32-H2 DevKit, attach as
a SED to the border router and measure the light-sleep floor with Thread state retained
(`SPIKE-SED-SLEEP`).
If bare-metal light sleep with the radio cannot hold the SED attachment, the fallback is
ESP-IDF's OpenThread on the same hardware. The PCB does not change either way.

### D-04 No OTA

Owner decision for battery life. Pulling an image over 802.15.4 costs minutes of radio time
per update, and a rollback-safe OTA adds a health-confirmation path to every boot. Firmware
updates go over USB-C, which the device already needs for charging. The partition layout is
the stock single factory app, and plain `espflash` is the flasher.

### D-05 Network: Thread SED on an Omni-owned border router

Slate joins a Thread network formed by an OpenThread Border Router (OTBR) running on boris
with a USB radio co-processor. **The study assumed "a Thread border router" without
saying whose.** Apple's border routers do not hand their network credentials to a
non-Matter device without an iOS app reading them with the user's consent, and reaching
boris from their off-mesh routes would depend on Router Advertisement route options being
accepted on a Docker host. An OTBR on boris owns its dataset, and the Omni container
reaches the mesh through a route on the same host. Two Thread networks on different
channels coexist without trouble. See [infrastructure.md](infrastructure.md).

**Gate G-03:** choose the RCP hardware (BOM candidates) and stand up OTBR on boris before
the firmware network bring-up (`OTBR-BRINGUP`).

No Matter: Omni owns both ends of the protocol, so Matter would add commissioning
and data-model machinery without reducing power or board complexity. No raw 802.15.4: the
packet energy it saves is negligible next to the panel, and it would mean owning
association, security and a gateway.

### D-06 Provisioning over the USB console

The Thread active dataset, the server address and the device id are written over the USB
Serial/JTAG console and stored in flash. No BLE, no buttons, no joiner commissioning.

### D-07 Device-initiated checks, server-directed interval

The device polls; the server never pushes. A pushed "frame changed" hint would replace a
CoAP round trip with a bare data poll and save roughly 2 to 3 mJ per minute, 0.3 to 0.45 Wh
per year, about 1% of the 40 Wh 100-refresh/day budget, at the cost of making boris reach into the
mesh and handling lost hints. Instead every check response carries the next check
interval, so the server can slow the device overnight or while nothing is scheduled to
change.

### D-08 CoAP over UDP, binary payloads

CoAP (RFC 7252) with Block2 (RFC 7959) for frames, encoded and parsed in Rust from the
shared `slate-protocol` crate. Payloads are fixed little-endian layouts so the device needs
no allocator. Security is Thread's link-layer encryption plus the server accepting only the
mesh prefix and provisioned device ids; no DTLS in Rev A.

### D-09 Server-side rendering and refresh policy

Omni renders every frame in the panel's native format and decides when a new
version exists: it coalesces changes, holds quiet hours, and enforces a daily refresh cap.
Because refreshes dominate energy, this policy is the battery-life control and belongs
where the content is known. Every frame includes a rendered "updated at" time, so a stale
image is visible without the device drawing.

### D-10 Row codec, verified before the waveform

Rows are 240 bytes of native two-bit data, each sent as raw, PackBits or repeat-previous.
The device keeps two row buffers, never a frame buffer, and needs no PSRAM. It checks the
CRC-32 of the decoded frame before starting a waveform; a corrupt or truncated transfer
costs radio time and RAM writes but never a refresh.

### D-11 Rx-on-when-idle during transfers

A SED receives through its parent's indirect queue, one poll per message. For the duration
of a frame fetch the device switches its link mode to rx-on-when-idle and returns to SED
polling afterwards. Hypothesis to confirm in `SPIKE-TRANSFER-ENERGY`.

### D-12 Light sleep by default, deep sleep for long absences

ESP32-H2-MINI-1 specifies 25 uA in its lowest light-sleep state and 7 uA in deep sleep. The
18 uA difference is worth about 3.6 mJ per minute at 3.3 V, less than a deep-sleep wake
plus Thread state restoration plausibly costs. Light sleep is the normal 60 s mode; deep
sleep is for long offline backoff, exhausted battery and shipping. `SPIKE-LIGHT-VS-DEEP`
settles it.

### D-13 One 3.3 V rail; panel power removed between updates

TPS63900 buck-boost (75 nA IQ) supplies everything. TPS22917 switches the panel rail off
after the controller's sleep command, so the panel's 1 uA deep sleep does not run all day.
Before the switch opens, every panel signal is driven low and stays low, so no GPIO
back-powers the unpowered panel through its protection diodes. No dynamic voltage scaling
in Rev A.

### D-14 Charger: BQ25185 at 500 mA

4.2 V regulation, 500 mA input limit, 500 mA charge. A 15 Ah pack takes about 30 h plus
taper from empty; acceptable for a device charged a few times a year, and within default
USB power, so no USB-C current detection or PD controller.

### D-15 Battery: protected 1S pouch with NTC, 15 Ah nominal

Protection lives in the pack. The board keeps a DNP footprint for BQ29700 and two FETs for
an unprotected cell. One pouch, or a factory-matched parallel pack with one protector and
one NTC; never cells paralleled on the board.

**Gate G-04 (before buying the pouch):** measured energy from the spike and panel tests.
At 100 refreshes per day, 15 Ah covers a year in the nominal model and 20 Ah in the
conservative transfer case. At 50 refreshes per day, 10 Ah covers a year with margin in
the nominal model and only just in the conservative one (10.5 Ah). Closes on
`SPIKE-CHECK-ENERGY`, `SPIKE-TRANSFER-ENERGY` and `PANEL-FULL-ENERGY`.

### D-16 Stay awake on USB power; recovery pads

USB Serial/JTAG disappears while the H2 sleeps. A VBUS divider tells the firmware that USB
power is present, and it then stays awake so the console and flashing always work. EN and
BOOT (GPIO9) are test pads for recovery; there are no user buttons or LEDs.

### D-17 Battery sensing by voltage only

A 3.3 MOhm / 1.0 MOhm divider into the ADC, with 10 nF at the pin, draws about 0.86 uA. The device reports
millivolts and a coarse band; the server renders "charge soon". No fuel gauge.

## Rejected for Rev A

Matter; raw 802.15.4; Wi-Fi or ESP32-C6; external PSRAM, flash, RTC or 32 kHz crystal; a
fuel-gauge IC; a separate e-paper driver board (the HV network is a handful of discretes);
image codecs beyond row RLE; delta or rectangle updates (the panel still refreshes
globally); keeping panel RAM powered; dynamic rails; USB PD; status LEDs and buttons; OTA.

## Open questions

- Dashboard content and layout: a server-side design task, independent of the hardware.
- Fast refresh as the default waveform: only after `PANEL-FAST-QUALITY` passes.
- Mounting and enclosure: after the pouch is chosen.

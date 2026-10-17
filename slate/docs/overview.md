# Overview

Omni Slate is a battery-powered 13.3-inch four-colour e-paper display that shows a
dashboard rendered by Omni. It hangs on a wall, refreshes when the content changes,
and should run six months to a year between charges.

## Rev A architecture

```text
Omni (boris)                     Slate
  renderer -> frame versions             ESP32-H2 Thread sleepy end device
  CoAP endpoint  <--- UDP/IPv6 --->      check every ~60 s, fetch on change
  OpenThread Border Router + RCP         SSD2677 panel on a switched 3.3 V rail
                                         1S protected LiPo pouch, USB-C charge
```

- **Display:** Good Display GDEY133F91, 960 x 680, black/white/red/yellow E Ink Spectra
  3100, SSD2677 controller on the panel, 4-wire SPI. Every visible update is a whole-panel
  waveform (25 s full, 20 s fast candidate).
- **MCU and radio:** ESP32-H2-MINI-1-H4S (4 MB flash, 320 KB SRAM, certified antenna).
- **Network:** Thread sleepy end device joined to an OpenThread Border Router that Omni
  owns on boris. A small CoAP protocol; no Matter, no Wi-Fi, no BLE commissioning.
- **Rendering:** entirely on the server. The device streams pre-encoded rows into panel
  RAM and never stores a frame.
- **Power:** BQ25185 charger with power path, TPS63900 buck-boost to one 3.3 V rail,
  TPS22917 load switch that removes panel power between updates.
- **Battery:** 1S protected pouch with NTC, 15 Ah nominal, mechanical room for 20 Ah.
- **Firmware:** bare-metal Rust on esp-hal with the esp-rs `openthread` crate. No OTA:
  updates go over the same USB-C port that charges it.

The panel refresh dominates the energy budget: 2.7 J per full update at the panel, against
about 1.8 Wh per year for everything else (see [power-budget.md](power-budget.md)). The
largest battery-life lever is therefore how often the server publishes a new frame, which
is why the refresh policy lives on the server ([protocol.md](protocol.md)).

## Dossier

| Doc | Holds |
| --- | --- |
| [STATE.md](STATE.md) | Current work, next step, candidates not chosen |
| [decisions.md](decisions.md) | Locked decisions, departures from the research study, open gates |
| [electrical.md](electrical.md) | Power tree, panel interface, pin map, PCB handoff |
| [firmware.md](firmware.md) | Device behaviour contract and firmware stack |
| [protocol.md](protocol.md) | Wire contract and server refresh policy |
| [infrastructure.md](infrastructure.md) | Border router and Omni server side |
| [power-budget.md](power-budget.md) | Energy model and battery sizing |
| [mechanical.md](mechanical.md) | Envelope reservations for the enclosure |
| [build.md](build.md) | Build sequence and procurement gates |
| [sources.md](sources.md) | Datasheets and the origin of each specified number |

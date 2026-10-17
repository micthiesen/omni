# Omni Slate

A battery-powered 13.3-inch four-colour e-paper display for Omni dashboards:
Good Display GDEY133F91 panel, ESP32-H2 on Thread, server-side rendering, months to a year
per charge. Status: specified and scaffolded; nothing built or bought.

- [`docs/`](docs/overview.md): the design dossier (current work in
  [docs/STATE.md](docs/STATE.md))
- [`bom/`](bom/README.md): parts with purchase tracking
- [`testing/`](testing/README.md): acceptance matrix with results
- [`firmware/`](firmware/README.md): Rust `no_std` ESP32-H2 firmware (scaffold)
- [`pcb/`](pcb/README.md): tscircuit mainboard source (outline and capture plan)

Gate: `sh scripts/check.sh`.

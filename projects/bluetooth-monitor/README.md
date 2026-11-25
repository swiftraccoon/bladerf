# Bluetooth monitor

Record Bluetooth packet candidates with a bladeRF and independently validate
BLE packets. The tool also inspects saved PCAPs and analyzes retained IQ.

## Quick start

[Initialize the workspace submodules](../../repos/README.md), then inspect a
saved capture from the repository root.

```sh
cargo run --locked -p bluetooth-monitor --release -- inspect \
  captures/bluetooth-session/candidates.pcap --output artifacts/bluetooth-inspect
```

Use a fresh output directory. For capture or IQ replay, first build a
[supported native decoder](docs/guide.md#build-the-native-engines).

## Guides

- [Capture, saved-IQ replay and burst analysis](docs/guide.md#capture-and-inspect)
- [Decoder scope and validation](docs/guide.md#decoder-scope)
- [Bandwidth, timing and signal metadata](docs/guide.md#bandwidth-timing-and-signal-metadata)

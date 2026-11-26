# Digital-signal monitor

Survey saved bladeRF IQ and run the workspace's Wi-Fi, Bluetooth and
IEEE 802.15.4 decoders. Use rolling or finite RX1 monitoring for live input.

## Quick start

Build the [monitor and its decoders](docs/guide.md) from the repository root,
then point `analyze` at an SC16_Q11 capture with its adjacent JSON sidecar.

```sh
target/release/digital-monitor analyze captures/session/iq.sc16q11 \
  --output artifacts/digital-monitor/session
```

Use a new output directory. The
[saved-input requirements](docs/guide.md#analyze-an-existing-capture) describe
the supported sample rate, bandwidth and tuning.

## Guides

- [Identify channels in a saved sweep](docs/guide.md#identify-channels-in-a-saved-sweep)
- [Rolling RX1 monitoring](docs/guide.md#segmented-rx1-monitoring)
- [Finite RX1 snapshots](docs/guide.md#finite-rx1-monitoring)
- [Recurring waveform observations](docs/guide.md#inventory-recurring-waveform-family-bins)

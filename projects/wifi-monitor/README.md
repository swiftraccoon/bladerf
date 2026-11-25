# Wi-Fi monitor

Capture bounded bladeRF IQ, replay 20 MSps recordings, and inspect recovered
802.11 frames with independent FCS checks.

## Quick start

Prepare the [pinned decoder and build dependencies](docs/guide.md#build-and-check).
From the repository root, build the receiver and try a saved recording.

```sh
cargo build --locked --release -p wifi-monitor -p bladerf-lab
target/release/wifi-monitor decode-iq captures/wifi-session/iq.sc16q11 \
  --format sc16q11 --frequency-mhz 2412 --output artifacts/wifi-decoded
```

Use the recording's actual center frequency and a new output directory.
`decode-iq` runs without a radio.

## Guides

- [Capture, survey and repeated monitoring](docs/guide.md#capture-and-inspect)
- [Saved-IQ replay, including HE20 and VHT20](docs/guide.md#replay-saved-iq)
- [Output formats and failure behavior](docs/guide.md#outputs-and-failure-handling)
- [Decoder coverage](docs/guide.md#decoder-coverage)

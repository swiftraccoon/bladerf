# bladeRF LoRa file worker

Decode LoRa physical-layer frames from channel-centered CF32 IQ. The worker
records packets whose payload CRC and observed sync word match.

Apply the [pinned FutureSDR patch](docs/guide.md#build), then build from this
directory with the pinned nightly.

```sh
cargo build --locked --offline --release
target/release/bladerf-lora-worker decode INPUT.cf32 \
  --channel-hz 906875000 --bandwidth-hz 250000 --spreading-factor 11 \
  --sync-word 43 --max-frames 64 --output OUTPUT.json
```

See the [worker guide](docs/guide.md) for the input rate, limits, and receipt
fields. The worker is licensed under [GPL-3.0-only](LICENSE).

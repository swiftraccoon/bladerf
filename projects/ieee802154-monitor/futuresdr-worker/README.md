# FutureSDR IEEE 802.15.4 file worker

Demodulate a complete 4 MSps channel-centered CF32 file and write raw
IEEE 802.15.4 PSDU candidates.

## Quick start

Apply the [required FutureSDR patches](docs/guide.md#build-and-run) in the
documented order. Build from this directory to use the pinned nightly.

```sh
cargo build --locked --offline --release
target/release/ieee802154-futuresdr-worker INPUT.cf32 \
  --output OUTPUT.i154raw --channel 17 \
  --max-input-mib 256 --max-frames 1000000 --max-output-mib 64
```

Use the [independent inspector](../docs/guide.md#inspect-demodulator-candidates)
to validate candidates. The [worker guide](docs/guide.md) covers decoder lanes,
carrier correction, source positions and file publication.

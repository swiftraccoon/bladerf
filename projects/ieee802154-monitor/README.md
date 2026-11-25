# IEEE 802.15.4 monitor

Extract 4 MSps channel files from a saved 20 MSps bladeRF capture. A separate
FutureSDR worker demodulates those files for independent FCS inspection.

## Quick start

[Initialize the workspace submodules](../../repos/README.md), then run from the
repository root with a C toolchain installed for `ring`.

```sh
cargo build --locked --release -p ieee802154-monitor
target/release/ieee802154-monitor channelize captures/session/iq.sc16q11 \
  --channels 16,17,18 --output artifacts/ieee802154-channelize/session
```

The IQ file needs its adjacent acquisition sidecar. Choose channels that fit
its sampled band and use a new output directory.

## Guides

- [Channel selection and derived file formats](docs/guide.md)
- [FutureSDR worker quick start](futuresdr-worker/README.md)
- [Inspect candidates and verify FCS](docs/guide.md#inspect-demodulator-candidates)
- [Compare carrier offset hypotheses](docs/guide.md#compare-carrier-offset-hypotheses)

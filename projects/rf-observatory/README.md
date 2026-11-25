# RF observatory

Record and survey wideband bladeRF IQ, run a four-channel FM reference receiver,
and verify the FPGA channel bank and framed transports.

## Quick start

[Initialize the workspace submodules](../../repos/README.md), then build the host
tools and run the device-free diagnostic from the repository root.

```sh
cargo build --locked --release -p rf-observatory
target/release/rf-observatory doctor
```

For live capture, also build [bladerf-lab](../../tools/bladerf-lab/README.md)
with the [native receive stack](../../docs/SETUP.md).

## Guides

- [Wideband capture and spectrum analysis](docs/guide.md#record-and-inspect-a-wideband-window)
- [FM reference receiver](docs/guide.md#select-several-fm-stations)
- [Profile-1 and Profile-2 transport workflows](docs/guide.md#custom-fpga-transport)
- [FPGA contract, build and verification](hdl/README.md)

# RF observatory

Record and survey wideband bladeRF IQ, sweep frequency ranges, run a four-channel
FM reference receiver, and verify the FPGA channel bank and framed transports.

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

- [Plan and capture a frequency sweep](docs/guide.md#sweep-a-frequency-range)
- [Find activity and save IQ clips from a sweep](docs/guide.md#triage-a-saved-sweep)
- [Wideband capture and spectrum analysis](docs/guide.md#record-and-inspect-a-wideband-window)
- [APRS packet-radio replay](docs/guide.md#decode-aprs-packet-radio)
- [Sensor identification from saved IQ](docs/guide.md#identify-433-mhz-ook-sensors)
- [LoRa preamble probing and packet decoding](docs/guide.md#probe-a-lora-channel)
- [FM reference receiver](docs/guide.md#select-several-fm-stations)
- [Profile-1 and Profile-2 transport workflows](docs/guide.md#custom-fpga-transport)
- [FPGA contract, build and verification](hdl/README.md)

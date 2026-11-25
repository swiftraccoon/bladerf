# bladeRF micro xA9 lab

A Rust workspace for receiving and analyzing signals with the bladeRF 2.0 micro
xA9. It includes passive monitors, saved-IQ tools, and FPGA reference designs.

## Quick start

Follow the [native setup guide](docs/SETUP.md), then run from the repository root.

```sh
cargo run --locked -p bladerf-lab --release -- doctor
cargo run --locked -p bladerf-lab --release -- --help
```

Use the [handbook](docs/handbook/index.md) for hardware background and the
[workspace guide](docs/workspace-guide.md) for capture examples, setup and validation.

## Find a component

| Directory | Contents |
| --- | --- |
| [tools](tools/README.md) | Receiver CLI, catalog tools, and FPGA process adapter |
| [crates](crates/README.md) | Shared Rust libraries for Profile-2 process communication |
| [repos](repos/README.md) | Pinned upstream Git submodules |
| [patches](patches/README.md) | Changes applied to pinned upstream sources |
| [vendor](vendor/README.md) | Checked-in third-party sources |

Monitoring projects include the [RF observatory](projects/rf-observatory/README.md),
[digital monitor](projects/digital-monitor/README.md),
[Wi-Fi monitor](projects/wifi-monitor/README.md),
[Bluetooth monitor](projects/bluetooth-monitor/README.md), and
[IEEE 802.15.4 monitor](projects/ieee802154-monitor/README.md).

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and verification.

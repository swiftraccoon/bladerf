# Shared Rust crates

These libraries define the shared Profile-2 process protocols and the
Remote ID payload parser used by the Bluetooth and Wi-Fi monitors.

| Crate | Role |
| --- | --- |
| [`bladerf-profile2-ipc`](bladerf-profile2-ipc/README.md) | Register control and session shutdown |
| [`bladerf-profile2-loader-ipc`](bladerf-profile2-loader-ipc/README.md) | Guarded FPGA image transitions |
| [`bladerf-profile2-session-ipc`](bladerf-profile2-session-ipc/README.md) | Capture and stock image restoration in one session |
| [`bladerf-profile2-recovery-ipc`](bladerf-profile2-recovery-ipc/README.md) | Stock image recovery in a fresh process |
| [`bladerf-remote-id`](bladerf-remote-id/README.md) | Basic ID in Bluetooth and Wi-Fi Beacon payloads |

[Initialize the workspace submodules](../repos/README.md), then run the
crate-specific test commands from the repository root. Generate the API
references under `target/doc/` with the following command.

```sh
cargo doc --locked --offline --no-deps \
  -p bladerf-profile2-ipc -p bladerf-profile2-loader-ipc \
  -p bladerf-profile2-session-ipc -p bladerf-profile2-recovery-ipc \
  -p bladerf-remote-id
```

The [contributor guide](../CONTRIBUTING.md) covers workspace layout and
verification.

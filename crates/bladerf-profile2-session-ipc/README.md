# Profile-2 capture session IPC

This library defines the protocol for the RF observatory's guarded capture
session. One adapter process owns the device handle through custom image
loading, receive configuration, capture, stock restoration, and shutdown.

[Initialize the workspace submodules](../../repos/README.md), then run the host
tests from the repository root.

```sh
cargo test --locked -p bladerf-profile2-session-ipc
```

See the [crate reference](src/lib.rs) for the session contract and a receive-plan
example. The [RF observatory guide](../../projects/rf-observatory/README.md)
covers the guarded capture workflow.

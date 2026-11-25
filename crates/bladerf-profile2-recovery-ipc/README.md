# Profile-2 recovery IPC

This library defines the stock-only recovery protocol used by the RF
observatory. A fresh adapter process restores the pinned stock image, verifies
its identity, and shuts down.

[Initialize the workspace submodules](../../repos/README.md), then run the host
tests from the repository root.

```sh
cargo test --locked -p bladerf-profile2-recovery-ipc
```

See the [crate reference](src/lib.rs) for the recovery lifecycle and a fixture
request example. The [adapter guide](../../tools/bladerf-profile2-adapter/README.md)
covers the recovery process check.

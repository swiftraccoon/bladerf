# Profile-2 register IPC

This library defines the bounded request and response protocol for Profile-2
register control. The RF observatory uses it to read status, write the four
shadow words, commit their values, and shut down the adapter session.

[Initialize the workspace submodules](../../repos/README.md), then run the host
tests from the repository root.

```sh
cargo test --locked -p bladerf-profile2-ipc
```

See the [crate reference](src/lib.rs) for a frame round-trip example and the
[adapter guide](../../tools/bladerf-profile2-adapter/README.md) for process
behavior and fixture commands.

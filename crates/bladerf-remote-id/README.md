# OpenDroneID Basic ID

The Bluetooth and Wi-Fi monitors use this crate to read Basic ID
observations from captured OpenDroneID broadcasts.

Run the host tests from the workspace root.

```sh
cargo test --locked -p bladerf-remote-id
```

See the [crate reference](src/lib.rs) for the wire inputs, output fields,
and a parsing example.

# FPGA image staging

## Immutable image staging

The image-transition worker must stage both FPGA images after it has
started but before it opens a device. The device-free command below exercises
that exact safe-Rust staging code and then discards the descriptors:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-profile2-images \
  --custom-image /absolute/path/to/rf-observatory-profile2-xA9.rbf \
  --custom-sha256 "$PROFILE2_IMAGE_SHA256" \
  --custom-bytes "$PROFILE2_IMAGE_BYTES" \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

Set `PROFILE2_IMAGE_SHA256` and `PROFILE2_IMAGE_BYTES` from the strict
RF-observatory image verification result. The stock role is frozen independently in the adapter as
12,858,972 bytes with the SHA-256 recorded in the
[workspace image catalog](../../../config/images.sha256),
`3004467110e2b2790c8f6128f998c2149905a88eed91a87f11af3db524d60c65`;
the command accepts no replacement stock digest or size. Each source must be an
absolute path to a nonempty regular file no larger than 16 MiB. The final path
component is inspected directly without following a symlink; parent components
retain ordinary Linux path resolution.

For each role, the stager pins the source inode, requires the exact size, and in
one pass copies and hashes the bytes into a separate anonymous file. It rewinds
the copy and applies and verifies the Linux write, grow, shrink and further-seal
seals. The typed owner retains both the descriptor and its internally constructed
`/proc/self/fd` C string. A synchronous `bladerf_load_fpga` call can see
only that live sealed path. A fresh recovery worker must stage stock again in its
own process; it must not inherit or trust a failed primary worker's descriptor.

This command does not load libbladeRF, open or enumerate a device, use USB, load
an FPGA, configure RF, capture, enable a bias tee, write flash or transmit. A
successful run is evidence for exact immutable staging only, not image provenance
by itself or device behavior.

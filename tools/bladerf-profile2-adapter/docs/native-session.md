# Native session and recovery children

These child contracts use the [loader-neutral libbladeRF build](native-library.md#loader-neutral-image-transition-build).

## Hidden native complete-session child

RF-observatory's
[guarded capture procedure](../../../projects/rf-observatory/docs/guide.md#guarded-profile-2-ram-capture)
launches `session-native-primary` as its role-fixed native child. Processing a
valid native-mode Hello includes opening the selected USB device. Later
requests can RAM-load the custom and stock FPGA images, configure the selected
RX channel and capture samples.

The child takes all authority as direct process arguments:

```text
bladerf-profile2-adapter session-native-primary \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 <64-lowercase-hex-library-digest> \
  --custom-image /absolute/path/to/rf-observatory-profile2-xA9.rbf \
  --custom-sha256 <64-lowercase-hex-custom-digest> \
  --custom-bytes <exact-custom-byte-count> \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf \
  --serial <32-lowercase-hex-device-serial>
```

It reads no supplied path and invokes no library or device operation before a
complete session-family, version-1, request-ID-one Hello for
`native_primary`. Serial syntax is then rejected before any path access. The
child validates both image expectations, stages and seals the exact custom and
frozen stock roles, and only then hash-qualifies and maps the B2L1 library. The
successful Hello attests libbladeRF 2.6.0, marker `0x42324c01`, the supplied
library and custom facts, and the frozen stock digest and byte count.

The retained `LoaderDynamicCalls` table moves into one
`PrimaryLoaderWorker`, rather than being held beside a model worker. That
worker opens `libusb:serial=<complete-serial>` and retains its sole handle,
both sealed image descriptors and the library mapping through custom load and
identity, typed control, receive configuration/readback, capture, RX stop,
same-handle stock restoration, stock verification and orderly close. Its safe
traits expose no TX stream or enable, flash operation, bias-tee mutation,
loopback mutation, discovery or arbitrary device selector.

An orderly close from the untouched or verified-stock state releases the
handle before unloading the retained library. If a live handle remains in any
other state, including a terminal callback result or protocol abort, Drop makes
no further device call and intentionally retains the B2L1 call table, mapped
library and sealed library backing file until kernel process teardown. This
prevents `dlclose` while libbladeRF or its synchronous-stream worker may still
execute code from that mapping; the resulting process-scoped leak is part of
the fresh-process recovery boundary.

A normal session restores stock on its one handle. After an ambiguous load,
identity failure or other terminal worker error, the supervised parent reaps
the primary and starts the distinct recovery child below with independently
staged stock. Device-free primary tests cover help, EOF, wrong family or mode,
and malformed serial handling before file or device access.

## Dedicated fresh-recovery process

Fresh recovery uses the independent `bladerf_profile2_recovery` version-1
family rather than changing loader/session version 1 or presenting a native
worker as a fixture. Its only operations are Hello, frozen-stock dispatch,
stock verification and orderly shutdown. The value-only Hello bundle contains
libbladeRF 2.6.0, the selected library digest, B2L1 marker `0x42324c01`, and the
frozen stock digest/size; it contains no custom-image field, path or serial.

The public CPU-only checker starts a hidden `recovery-fixture` child. That child
independently stages stock, maps and retains B2L1 after staging, then gives its
one `RecoveryLoaderWorker` deterministic model calls:

```text
bladerf-profile2-adapter check-profile2-recovery-process \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 <64-lowercase-hex-library-digest> \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

The hidden native counterpart has this fixed direct interface:

```text
bladerf-profile2-adapter recovery-native \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 <64-lowercase-hex-library-digest> \
  --serial <32-lowercase-hex-device-serial> \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

Hiding is not a safety boundary: an exact native-mode Hello constructs the
backend, opens the selected USB device and allows a later request to RAM-load
stock. Serial syntax is rejected before any supplied path access. The child
stages a new sealed stock snapshot before mapping the library, then moves
`LoaderDynamicCalls` into the stock-only worker. It exposes no custom image,
Wishbone/register, RF, capture, TX, flash, bias-tee, loopback, discovery or
arbitrary-selector operation. As with the primary, a live non-orderly handle
retains the library mapping until kernel process teardown rather than risking
`dlclose` while foreign code may still run.

The existing stock-identity contract combines the exact staged/dispatched
stock bytes and successful load with configured state, host load source and
FPGA version 0.16.0 readback. It is not cryptographic FPGA-configuration
readback. Stronger proof requires a separate stock-specific device mechanism.
The ordinary test suite reaches only help, EOF, wrong-family/mode,
malformed-serial and fake-worker paths; it never drives a valid native recovery
lifecycle.

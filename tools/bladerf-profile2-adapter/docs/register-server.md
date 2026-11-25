# Profile-2 register protocol and server

## CPU fixture

The fixture reads the versioned, length-prefixed protocol from standard input
and writes only framed responses to standard output. Diagnostics use standard
error. The session is single-threaded, requires `hello` first, accepts strictly
consecutive request identifiers beginning at one and terminates after any error.
The client must use one-request/one-response lockstep; pipelining is unsupported.
At most `MAX_SESSION_REQUESTS` (currently 256) requests are handled, and a
following request receives the terminal request-limit response. The constant is
exported by `bladerf-profile2-ipc` so parent-side bounded workflows can reserve
space for readback and explicit shutdown. The server uses blocking standard I/O,
so its owning client must impose deadlines and reap the child if a peer stalls.
A fully decoded request receives exactly one correlated response. Malformed or
truncated framing and end-of-input before shutdown have no request identifier,
so they terminate the process without synthesizing a response.

```sh
cargo run --locked -p bladerf-profile2-adapter -- fixture
```

The command above waits for binary protocol frames; it is intended for the
RF-observatory client and automated tests, not interactive terminal use.

## Native register server

The native server takes the
[configuration-neutral library](native-library.md#specialized-bladerf-2-open-build),
its exact digest and a complete lowercase 32-hex-digit serial.

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  native \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --serial 0123456789abcdef0123456789abcdef
```

The placeholders are intentionally not local device credentials. The process
does not touch USB until it receives a valid protocol `hello`. It then
unconditionally requires marker V1 from the same sealed library used for all
calls, opens `libusb:serial=<complete-serial>` and requires:

- an exact returned serial and board name `bladerf2`;
- FPGA size xA9, configured state `1`, host-load source and numeric version
  `0.16.0`; and
- exact profile-2 ID `0x52460201` and capabilities `0x00001f04` read through
  Wishbone.

Every post-open failure closes the one handle. Only after all gates pass can the
framed protocol read its typed register allowlist, write one of four shadow
words, or issue the fixed commit word. There is no generic address, FPGA-load,
RF configuration, capture, bias-tee or TX operation. Opening still claims the
USB interface and changes transport alternate settings; profile identity reads
can block on an incompatible image, which is why this code remains inside the
deadline-controlled child process. Forced process termination relies on the OS
to release the USB claim because destructors cannot run after a kill.

The RF-observatory `verify-profile2-native-adapter` action requires
absolute canonical adapter/library files, exact digest and full serial, removes
`LD_PRELOAD`, `LD_AUDIT` and `LD_LIBRARY_PATH` before starting the child, and
requires a native-mode hello under the same finite transaction deadline and
process-cleanup contract as the fixture. Loader/system dependencies remain part
of the trusted runtime rather than evidence covered by the main-library hash.

The parent accepts a valid non-exhausted current applied epoch and verifies the
next correlated epoch, so verification is not restricted to a fresh image at
epoch zero. Ctrl-C is checked before each later IPC operation; a request already
in progress may continue until its per-request deadline before child cleanup.
Completed writes are not rolled back: a partial shadow bank remains staged, and
an in-flight commit can leave the outcome indeterminate. Keep RX stopped and
reload/reset or use a separately qualified reconciliation procedure before a
retry after interruption.

Before using the native register server, verify the matching Profile-2 image
manifest, load the image in RAM and stop RX.

Run the CPU-only Controller/client/server check from the workspace root.

```sh
cargo build --locked -p bladerf-profile2-adapter
cargo run --locked -p rf-observatory -- verify-profile2-adapter
```

After the process fixture succeeds, the observatory runs an in-process
guarded-session model for same-handle stock restoration and fresh-worker
recovery after an ambiguous custom load. The
[device-free loader check](loader-qualification.md) exercises the typed loader
worker separately.

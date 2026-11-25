# Device-free session qualification

These checks use the [loader-neutral library](native-library.md#loader-neutral-image-transition-build)
and the [staged image roles](image-staging.md).

## Device-free complete session-worker qualification

The next command retains the expanded receive-only B2L1 table while running one
complete image/control/receive/restoration epoch against deterministic
`ModelCalls`:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-profile2-session-worker \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --custom-image /absolute/path/to/rf-observatory-profile2-xA9.rbf \
  --custom-sha256 "$PROFILE2_IMAGE_SHA256" \
  --custom-bytes "$PROFILE2_IMAGE_BYTES" \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

The retained table adds exact typed Wishbone write, device-speed and read-only
RX/TX bias-tee checks, frequency, sample-rate, bandwidth and manual-gain
configuration/readback, a read-only loopback check, baseband RX-mux
set/readback, synchronous-stream, RX-enable and synchronous-RX declarations.
The private table necessarily retains libbladeRF's generic
`bladerf_enable_module` pointer, but `SessionCalls` fixes its use to the selected
RX1/RX2 channel and exposes no TX enable or TX stream. Flash, bias-tee or
loopback mutation, packet-stream and metadata-stream operations also remain
outside that safe surface. A real configuration callback must require
SuperSpeed, both bias tees off and loopback `NONE` before changing RF state.
The fake worker uses none of those device-facing pointers: it opens its model
handle, verifies the custom role, completes the typed stopped-RX register
transaction, models RX2 at 98.1 MHz, 20 MSps, 20 MHz bandwidth and 40 dB gain,
starts and completes capture, stops RX, restores the sealed stock role on the
same handle and verifies stock before close.

The modeled plan sizes its extent for 100,000 DATA records preceded by one
CONFIG record. At eight metadata-free SC16 Q11 transport slots per record, the
bounded trace covers 800,008 transport slots (samples) and 3,200,032 bytes. The
model writes and hashes that exact extent without retaining it as a capture
artifact. Its bounded-memory encoder emits one clean CONFIG containing the
applied epoch and phase words, then clean DATA records for the explicit RX input
with source index 19 followed by exact increments of 20. Focused tests check
both RX1 and RX2 record fields and frozen stream digests. Failures and unwinds
terminalize the worker before any callback; stock restoration is permitted only
before RX starts, after configured-but-disabled receive, or after a successful
stop.

The retained native receive sequence is factored behind a private fakeable ABI
seam. CPU tests pin its guard/configuration order, exact stream constants,
active readbacks, bounded chunking and selected-RX stop behavior. If RX enable
succeeds but an active readback fails, or if native capture, output writing or
output flush returns an error, the callback makes one best-effort disable of
that selected RX before returning. An initial enable failure issues no second
module call. Either result still leaves the worker terminal; a disable failure
is preserved with the primary diagnostic and recovery must use a fresh worker.

This command accepts no serial or device selector. Loading the selected shared
object runs its initializer, but the adapter calls only `bladerf_version` and
the B2L1 marker. All image-load, register, RF, stream, capture and stop events
are fake; no device, USB transfer, FPGA load, RF configuration, physical
capture, bias-tee, flash or TX operation occurs.

The device-free complete-session process below binds one fake handle to the
worker and transfers raw model evidence. The [native session and recovery
children](native-session.md) serve the supervised RF-observatory
`SessionDevice`/`SessionFactory` binding. One primary process owns image
loading, registers, RF configuration and capture.

## Device-free complete session process

The public checker below starts one role-fixed child and drives the complete
model session through a distinct version-1 protocol family:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-profile2-session-process \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --custom-image /absolute/path/to/rf-observatory-profile2-xA9.rbf \
  --custom-sha256 "$PROFILE2_IMAGE_SHA256" \
  --custom-bytes "$PROFILE2_IMAGE_BYTES" \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

Append `--maximum-pipe-stress` to this device-free command to run the explicit
maximum-size qualification. That opt-in mode selects 10,000,000 DATA records
and streams them into an unlinked mode-0600 temporary file while keeping only
fixed-size count/hash state in memory. It pauses the parent sink once after raw
streaming begins, then writes, flushes and synchronizes all 320,000,032 bytes
within the same 60-second capture transaction deadline. The forced parent
sink/reader pause is required to enter, last at least 250 ms and release before
success. It exercises the finite operating-system pipe under a reader stall;
the receipt does not claim a direct measurement of child-writer blocked time.
The system temporary directory may be disk-backed or a memory-backed
filesystem: at least 320,000,032 bytes must be free, and 1 GiB of free-space
headroom is recommended. On Unix, set `TMPDIR` to select a suitable disk-backed
location when the default temporary directory is memory-backed. The anonymous
file is removed automatically when its descriptor closes, including on failure.
This mode remains entirely model-driven and invokes no device API. It is not
part of ordinary tests or the default reference command.

The checker and its internal child command accept no serial or device selector.
The parent validates only expectation syntax before spawn. After an exact Hello,
the child independently stages both image roles, hash-qualifies and retains the
exact B2L1 table, and constructs one `PrimaryLoaderWorker<ModelCalls>` with one
model handle. The native `LoaderDynamicCalls` table is retained for the entire
session but is never passed to that worker. The same worker and handle traverse
custom load and identity, typed register reads and shadow writes, fixed commit,
RX2 configuration and readback, start and active readback, capture, stop, stock
restoration and identity, and orderly shutdown.

Capture uses a deliberately mixed stdout protocol. The child first writes an
exact `CaptureStarted` frame, then exactly the declared raw bytes, then a
`CaptureComplete` frame. A private child-side writer independently counts and
hashes bytes actually accepted by stdout and compares that evidence with the
worker report before completion can be framed. Once `CaptureStarted` has been
published, a backend failure, short or excessive output, digest mismatch, or
stdout failure terminates the child without an error frame; framed diagnostics
can therefore never be confused with raw capture bytes.

By default the parent creates a private temporary `File` and passes it to the
client by ownership; no writable path crosses IPC. The client independently
counts and hashes the raw stream while copying it into that sink. The checker
leaves the returned file untouched until RX stop, same-handle stock restoration,
stock verification, orderly shutdown and child reaping have all succeeded. It
then rewinds the file and independently reads, counts and hashes it a second time
before accepting the report. The maximum-pipe mode instead uses fixed-size
count/hash working state plus an anonymous file whose single
entered-and-released pause is included in the qualification receipt. Any earlier
failure drops the unvalidated sink.

The successful RX2 model emits one clean CONFIG plus 100,000 continuous DATA
records for the exact bounded extent: 800,008 transport samples and 3,200,032
bytes, with frozen SHA-256
`284863be84327eead3b5f9c4f2873f55f93664861b3c70693df62415002fa12e`.
CONFIG carries applied epoch 1, the exact four model phase words, the required
20 MHz source rate and RX2. DATA begins at source index 19, advances by 20 and
uses deterministic zero IQ payloads. This remains a CPU-generated fixture, not
device, USB, FPGA, RF or physical-capture evidence.

The maximum-pipe variant uses the same record contract for 10,000,000 DATA
records plus CONFIG: exactly 80,000,008 transport samples and 320,000,032 bytes.
Its parent, client and child hashes must agree with the frozen maximum-stream
digest
`0151cdb0719ce7e6671fa2650fed7f32db0767f5f4e9c1cc48fc8b1be3aa1d87`,
and the receipt reports capture elapsed time and remaining deadline headroom as
well as the observed reader-pause duration. Only after RX stop, same-handle stock
restoration, verification, shutdown and child reaping succeed does the checker
rewind the synchronized file and independently recount and rehash every byte.
The maximum qualification therefore covers the model encoder, child stdout IPC,
session client and realistic file write/flush/sync/reread work at full extent.
It does not exercise RF-observatory's named-stage publication, cancellation or
strict record parser; their existing smaller-extent coverage remains separate.

The client removes `LD_PRELOAD`, `LD_AUDIT` and `LD_LIBRARY_PATH`, permits one
outstanding request, applies a finite deadline to every transaction, and uses
private-process-group kill/reap fallback. Wrong family, version, request ID,
operation order, readback, raw extent, digest or response is terminal. The
complete state sequence is:

```text
Ready -> CustomUnverified -> CustomVerified -> ReceiveConfigured
      -> ReceiveActive -> CaptureComplete -> ReceiveStopped
      -> StockUnverified -> StockVerified -> Closed
```

Loading the selected shared object runs its initializer and transitive loader
dependencies. The adapter invokes only `bladerf_version` and the B2L1 marker;
every open, image-load, register, RF, capture, stop and close transition is a
model call. This command selects no device and performs no USB access, physical
FPGA load, RF configuration, physical capture, bias-tee operation, flash write
or transmission. It is process and release-boundary evidence for the fake
backend, not hardware qualification or a native production session.

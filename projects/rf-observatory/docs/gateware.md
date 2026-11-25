# FPGA channel bank and framed transport

The first FPGA stage is four parallel digital downconverters. Each has its own
32-bit phase accumulator and a third-order CIC decimator. The host performs the
remaining channel-select filter, decimation and FM demodulation. This stage uses
portable VHDL rather than licensed vendor FFT/DDS blocks.

VHDL is limited to synthesizable hardware and its hardware testbenches. It fits
the existing Nuand VHDL interface and Quartus synthesis flow directly. Maintained
host software, numerical reference models and verification orchestration use
Rust; see [the reference implementation](../src/dsp.rs) and
[GHDL verification runner](../src/gateware.rs).

## Numeric and streaming contract

`rf_channel_bank` accepts signed 16-bit interleaved-complex components as separate
I/Q ports. Normal bladeRF SC16 Q11 samples fit this range, while verification
also exercises the full signed 16-bit domain.

| Property | Definition |
| --- | --- |
| Input transfer | Rising clock edge with `s_valid=1` and `s_ready=1` |
| Reset | Synchronous, active high; clears all state and pending output |
| Phase configuration | Four unsigned 32-bit steps, stable throughout an epoch |
| Phase update | Lookup current phase, then add its step modulo 2^32 |
| Oscillator | Top 8 phase bits index 256 exact Q1.15 sine coefficients; cosine is a quarter-cycle offset |
| Downmix | `(I*cos + Q*sin, Q*cos - I*sin)`, signed 33-bit sums, arithmetic shift right by 15 |
| Integrators | Three cascaded signed 48-bit modular accumulators; each stage uses the current updated preceding stage |
| Decimation | 20 accepted input samples per output group |
| Combs | Three signed 48-bit modular differences with one output-period delay per stage |
| Normalization | Arithmetic right shift by 13, followed by signed 16-bit saturation |
| CIC gain | 20^3 / 8192 = 0.9765625 after normalization |
| Clipping | One output flag per channel, asserted if either component saturates |
| Source index | Zero-based last accepted input index: 19, 39, 59, … |

The source index advances on accepted samples, not on clock edges. It is not a
hardware RF timestamp or UTC clock. Input gaps introduce no numerical samples;
previously accepted samples continue through the pipeline. A new reset clears
all pending samples and starts a new index epoch. The unsigned 64-bit counter
wraps modulo 2^64; start a new epoch before crossing that boundary. Initial
outputs include the CIC's startup transient; downstream filtering has additional
settling and group delay.

The registered stages separate coefficient lookup, multiplication, mixed-sample
addition, each of the three integrators, each of the three combs, and output
normalization/saturation. The twentieth accepted sample produces output valid
after nine advancing clock edges; the earliest output transfer is the following
edge. This is pipeline latency in addition to sample-domain filter delay.
Individual integrators receive the updated preceding stage's value for the same
sample, delayed by its valid marker. Registers preserve all specified arithmetic
widths, rounding and wrap behavior. The oscillator, decimation counter and source
index advance only when an input is accepted. Pending samples drain even if no
further input arrives; an incomplete group of fewer than 20 samples produces no
output.

One complete output group is buffered. While output is valid and its consumer
is not ready, every pipeline stage freezes, the outputs and index remain stable,
and input readiness is low. Reset takes precedence over this freeze and discards
all pending samples and buffered output.
An upstream source that honors readiness loses no accepted input. An actual ADC
cannot pause its sample stream; `rf_channel_framer` reports that condition rather
than silently treating skipped samples as continuous reception.

At a physical input rate of 20 MSps, each CIC output is 1 MSps. The CIC alone is
not the complete FM channel-select filter. Its passband droop, alias rejection,
oscillator table quantization and host FIR response must be included when
assessing usable channel bandwidth and adjacent-signal rejection.

## Fixed profile 1

The board-integration wrapper currently has one compile-time profile:

| Channel | Phase step | Nominal offset at 20 MSps |
| --- | --- | --- |
| 0 | `0xb3333333` | -6 MHz |
| 1 | `0xe6666666` | -2 MHz |
| 2 | `0x1999999a` | +2 MHz |
| 3 | `0x4ccccccd` | +6 MHz |

The exact digital frequency is the signed phase step multiplied by the input
sample rate and divided by 2^32. The listed offsets are rounded descriptions.
Changing RF center frequency shifts all four channels together. Arbitrary
runtime phase programming is not implemented in this board wrapper; changing
these constants needs a newly built image and a corresponding profile contract.
The standalone bank and Rust model can test other phase configurations.

## Profile-2 control and FIFO-write integration

The tracked profile-2 path now joins the synthesizable register slave, control
CDC/start token, CONFIG-first framer, channel bank and two-slot packer through a
64-bit FIFO-write boundary. The portable wrapper is independently verified; the
target patch described below connects it to the pinned micro hosted top. Host
APIs use byte offsets, while the pinned Qsys interface removes the two low
alignment bits and presents these bus-word addresses:

| Host bytes | Bus word | Access | Value or meaning |
| --- | --- | --- | --- |
| `0x00` | 0 | RO | ID/layout `0x52460201` |
| `0x04` | 1 | RO | Capabilities `0x00001f04` |
| `0x10..0x1c` | 4..7 | RW | Four shadow phase words |
| `0x20` | 8 | WO | Commit magic `0x434d4954` (`CMIT`); reads return zero |
| `0x24` | 9 | RO | Status |
| `0x28` | 10 | RO | Last applied 32-bit epoch |
| `0x2c` | 11 | RO | Full epoch associated with the last outcome |
| `0x30..0x3c` | 12..15 | RO | Four last-applied phase words |

The capabilities low byte is the channel count, four. Bits 8 through 12 denote
stopped-RX commit, a fresh four-word bank after every accepted dispatch, full
outcome-epoch readback, an explicit apply result and one outstanding apply
request. Unknown reads return `0xbad0add0`; unknown and read-only writes have no
effect. Every address still receives exactly one ACK.

Status bit 31 is busy, bit 30 is outcome-valid, bit 29 is applied/success, bit
28 is the synchronized RX-active input, bits 27:24 are the reason, bits 23:20
are the current written mask and bits 19:0 are zero. Reason zero means none or
success; 1 is bad magic, 2 incomplete mask, 3 RX active, 4 epoch exhausted and
5 downstream apply rejection.

Every phase word is legal, including zero and duplicates. A commit is accepted
only when all four words have been written since reset or the previous accepted
dispatch, RX-active is clear, no request is busy and the epoch can advance.
Acceptance freezes the four words and next epoch, clears the mask and exposes
`apply_valid`; that payload remains unchanged until a later synchronized rising
event on `apply_done`. A stale or held-high result cannot complete a future
request without returning low and rising again. Writes while busy stage the next
bank. Any commit write while busy ACKs without changing the
pending request or status. Other rejections preserve shadows and mask and use
the priority bad magic, RX active, incomplete mask, then epoch exhaustion. An
apply rejection consumes the dispatched mask but does not update applied
readback, so a retry needs four fresh writes.

The standalone profile-2 control CDC captures the four phase words and epoch in
a 160-bit control-domain holding payload. A four-phase request/response **level**
handshake crosses only control signals and the one-bit result; it is not a toggle
protocol. The payload is stable for a full control clock before request assertion
and remains stable until response return-to-zero. Both control and RX clocks must
keep running for the exchange to finish.

The RX domain rechecks that RX-enable is low before accepting an apply. A later
successful stopped-RX apply replaces the previously armed words and epoch and
leaves one saturating token for a future start; it never queues a second start.
An RX-enable rising edge takes priority over an apply on the same RX clock. Every
such edge emits one start pulse: an armed token authorizes it and is consumed,
while a missing token produces an unauthorized start and no valid active bank.
RX-enable held high across reset release therefore becomes an unauthorized start
after RX reset releases. Stopping RX clears active-epoch validity but does not
re-arm a consumed token or discard an unused one.

Control and RX reset inputs are overlapping views of one PLL/image-lifetime
profile-init event. That reset drives the generated Qsys external reset, register
slave and control CDC, with synchronized release in the RX domain. Isolated
reset of only one side is not a recovery protocol. The FX3 `GPIO_SYS_RST` pulse,
ordinary RX reset and RX disable are stream-lifetime events: they clear the
framer, numerical bank and packer, but must not clear the applied bank or start
token. The target top also clears the physical sample FIFO on stop or profile
init reset. A successful register outcome means the RX domain accepted the
complete payload and armed the token; it does not mean that RX started or a
framed record was emitted.

Profile-init reset clears bus state, shadows, masks, pending apply, outcomes and
applied readback. The generated Qsys reset coherently resets its master; the
master's exported `wbm_wb_rst_i` conduit remains unused internally.
The multi-call write/commit/poll/readback sequence also requires one caller-side
transaction owner; thread-safe individual host calls alone do not make that
sequence atomic.

The safe Rust host-control core enforces that caller-side ownership with an
exclusive mutable register interface. It validates the literal ID, capabilities
and byte-offset map; requires stopped RX, an unexhausted epoch and no partial
bank; writes and reads back all four shadows; commits; polls to a bounded,
correlated outcome; and verifies applied epoch and words. An error never
authorizes capture and does not roll back partial register state. Strict capture
reconciliation additionally requires requested, applied, RFIC-readback and wire
CONFIG state to agree. This borrow does not provide global or cross-process
exclusion. The native server owns and serializes one handle for one session, but
cannot exclude another process from opening the same device.

The adapter package exposes a CPU fixture over bounded, versioned,
length-prefixed IPC. Its typed operations allow the frozen profile-2 reads, four
shadow writes, commit and shutdown. Requests and responses are lockstep with
consecutive IDs beginning at one, and a session handles at most 256 requests.
This fixture qualifies protocol framing and lifecycle only. A separate x86-64
Linux native command owns one device handle and implements the same allowlisted
register operations; it does not load an FPGA image, configure RF, capture or
transmit. Another device-free command hashes a selected libbladeRF build into a
sealed snapshot, resolves the pinned ABI and calls only its version function.

RF-observatory's `verify-profile2-adapter` action binds the existing Controller
to this fixture through the shared typed client. The client applies a finite
deadline to each write/read exchange, rejects inexact version/ID/result replies
and explicitly shuts down and reaps the child. The successful CPU check is not
native-register, USB, loaded-FPGA or RF evidence.

Profile 2 includes a control-to-FIFO-write path, a strict host parser and a
self-contained pinned target patch. The offline verifier checks the schema-2
image manifest and its evidence files. The guarded capture command requires an
explicit RX connector and loads the image into RAM for receive-only operation.

## Profile-1 raw framed-word format

`rf_channel_framer` emits eight complex transport slots per channel-bank output.
Each slot contains one 16-bit I word followed by one 16-bit Q word. The byte
representation for saved/native little-endian transport is I low byte, I high
byte, Q low byte, Q high byte. One record is 32 bytes.

| Slot | I word | Q word |
| --- | --- | --- |
| 0 | Magic `0x5246` | Magic `0x4f42` |
| 1 | Version and status flags | Profile ID `0x0001` |
| 2 | Source index bits 15:0 | Source index bits 31:16 |
| 3 | Source index bits 47:32 | Source index bits 63:48 |
| 4 | Signed channel 0 I | Signed channel 0 Q |
| 5 | Signed channel 1 I | Signed channel 1 Q |
| 6 | Signed channel 2 I | Signed channel 2 Q |
| 7 | Signed channel 3 I | Signed channel 3 Q |

Status-word bits are:

| Bits | Meaning |
| --- | --- |
| 15:12 | Format version, currently 1 |
| 11:8 | Channel 3..0 saturation flags |
| 7 | Sticky ADC-input loss or downstream sample-FIFO overflow |
| 6 | Sticky unsupported configuration or source-channel change |
| 5 | Physical RX2 source; zero denotes RX1 |
| 4:0 | Reserved, zero |

A record is snapshotted before serialization. Errors occurring during its
serialization appear in a subsequent record. Absence of a flag on one record
therefore cannot certify a complete acquisition. Check source indices, all
status words and native receive diagnostics. A reset clears sticky status and
begins a new epoch; it does not preserve a partially serialized record.

At 20 MSps input the nominal transport rate is 1 million records/s, or 8 million
complex transport slots/s (32 MB/s). These are custom framed words, not original
SC16 Q11 RF samples. In particular, header words may exceed the Q11 component
range. Capture them with a dedicated plain 16-bit path, preserve their bit
patterns and parse the framing explicitly. Normal IQ capture duration/count
logic based on the 20 MSps physical input is unsuitable for this transport.

## Profile-2 CONFIG-first wire format

Profile 2 retains the eight-slot, 32-byte record size and little-endian slot
encoding, but starts every valid epoch with exactly one CONFIG record. DATA
records follow only after that complete CONFIG has been accepted by the
framer's downstream ready/valid interface. Slot 0 remains magic
`0x5246,0x4f42`; slot 1 Q is profile ID `0x0002`.

| Slot-1 I bits | Meaning |
| --- | --- |
| 15:12 | Format version `2` |
| 11:8 | Channel 3..0 saturation flags; zero on CONFIG |
| 7 | Sticky input or downstream transport loss; zero on CONFIG |
| 6 | Configuration/start/source error |
| 5 | Latched physical source: RX2 when set, RX1 when clear |
| 4 | CONFIG when set, DATA when clear |
| 3:0 | Low nibble of the full CONFIG epoch |

The remaining slots depend on the record kind. A 32-bit word occupies one
complex slot with its low 16 bits in I and high 16 bits in Q. The DATA index is
64 bits, least-significant word first.

| Slot | CONFIG | DATA |
| --- | --- | --- |
| 2 | Epoch bits 15:0 / 31:16 | Source index bits 15:0 / 31:16 |
| 3 | Required rate bits 15:0 / 31:16 | Source index bits 47:32 / 63:48 |
| 4 | Phase word 0 low / high | Signed channel 0 I / Q |
| 5 | Phase word 1 low / high | Signed channel 1 I / Q |
| 6 | Phase word 2 low / high | Signed channel 2 I / Q |
| 7 | Phase word 3 low / high | Signed channel 3 I / Q |

The initial profile requires rate `20,000,000`, a nonzero 32-bit epoch and a
tag equal to the epoch's low nibble. A strict stream contains CONFIG at byte
zero exactly once, at least one DATA record, a fixed source and tag, no clipping,
loss or configuration indication, and source indexes `19, 39, 59, ...` without
64-bit wrap. The required-rate field is a contract to cross-check against RFIC
configuration; it does not measure the sample clock.

`rf_profile2_framer` accepts a start only when it is authorized, the active bank
is valid, the epoch is nonzero and neither configuration error nor transport
loss is present. An invalid or uncommitted start emits one diagnostic CONFIG
with bit 6 set, zero epoch/tag/phase words, the fixed required rate and the
source sampled at that start, then emits no DATA. The structural parser can
identify that diagnostic; strict stream validation rejects it. A configuration,
source, loss or second-start violation while CONFIG is being serialized fails
closed before DATA. During DATA, loss and configuration/start/source diagnostics
become sticky and are included in the next record snapshot; clipping describes
only that DATA snapshot. Same-edge diagnostics at a snapshot boundary are
included in that snapshot.

The framer holds its numerical bank in reset and keeps `s_ready` low until the
CONFIG slot-7 handshake. Samples presented before that boundary are deliberately
discarded and do not advance the index or assert loss. Thereafter `s_ready`
means actual bank admission. Reset discards a partial CONFIG or DATA record and
all bank state, so a new valid epoch again starts at index 19.

`rf_profile2_slot_packer` supplies the required admission boundary. Its empty
half-buffer can retain the first slot while the FIFO reports full. The second-
slot handshake and 64-bit FIFO write are the same event, with the second Q/I
word above the first Q/I word. Reset discards an unpaired slot. Consequently,
CONFIG slot 7 is also the fourth CONFIG FIFO write, and input admission opens
only on the following RX edge. Maintained simulation proves clean restart at
this boundary with test-driven `fifo_full`; a separate qualification using the
exact Quartus 23.1 DCFIFO model proves clear release, full backpressure, write
order and lower-word-first readout.

These results still do not prove CONFIG reaches a host-visible file. The
maintained verifier does not instantiate the Intel FIFO or GPIF, and the exact-
FIFO qualification stops at its read port. The GPIF starts a plain receive burst
only after a complete USB buffer has accumulated. A valid epoch eventually
crosses that threshold, but a diagnostic-only eight-word CONFIG remains below it
and is cleared on stop. Host integration must reject invalid setup before start
and must not promise a host-visible diagnostic until a defined partial-transfer
or padding contract exists. The target candidate also ties epoch-local
`transport_loss` low; a later source must be a clean event rather than a
stretched diagnostic level.

## Profile-2 Nuand integration

The [profile-2 integration patch](../../../patches/bladerf-rf-observatory-profile2.patch)
targets bladeRF source revision
[`fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47`](https://github.com/Nuand/bladeRF/tree/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47).
Apply the self-contained patch to an isolated source copy. In addition to the
top/QIP/RX changes, it creates these ten files under
`hdl/fpga/ip/nuand/rf_observatory/`:

- `rf_observatory_p.vhd`
- `rf_profile2_p.vhd`
- `rf_channel_bank.vhd`
- `rf_profile2_init_reset.vhd`
- `rf_profile2_wb_registers.vhd`
- `rf_profile2_control_cdc.vhd`
- `rf_profile2_framer.vhd`
- `rf_profile2_slot_packer.vhd`
- `rf_profile2_integration.vhd`
- `rf_profile2.sdc`

The patch maps both pinned Wishbone clocks to the 80 MHz system clock and drives
Qsys external reset from the separate PLL/image-lifetime initializer. It selects
exactly one enabled physical ADC, uses the normal RX clock domain, bypasses the
native `fifo_writer` only when a default-off profile-2 generic is selected, and
writes packed records directly into the existing mixed-width sample FIFO. The
default generic retains the native writer for other revisions. The sample FIFO,
FX3 read path and RFIC/TX control remain upstream components.

Metadata, packet, 8-bit, highly packed, alternate RX mux and armed-trigger modes
are unsupported. RX and GPIF are forced to plain metadata-free records, and the
GPIF remains raw in both directions; ordinary raw TX remains available, but TX
metadata and packet modes are unsupported in this image. A requested unsupported
RX mode contributes to the configuration diagnostic.

The SDC constrains the exact 160-source/160-destination bundled payload to the
complete 12.5 ns control-domain prepare interval and removes only its
inapplicable hold check. It cuts the asynchronous clear pins of the two-stage RX
reset release. It also cuts one reset source to exactly two read-side DCFIFO
`rdaclr` synchronizer clear pins, matching the FIFO's enabled
`READ_ACLR_SYNCH` behavior; cardinality guards make hierarchy drift an error.

Use the [Quartus build procedure](#quartus-baseline-and-custom-image-build)
to qualify an image for this target. Keep fitter, timing and assembly reports
with the image manifest. The [guarded capture procedure](guide.md#guarded-profile-2-ram-capture)
checks the loaded image and receive path on hardware.

## Profile-1 Nuand integration

The [integration patch](../../../patches/bladerf-rf-observatory-channel-bank.patch)
targets bladeRF source revision
[`fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47`](https://github.com/Nuand/bladeRF/tree/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47).
Apply it to an isolated source copy. Copy the three synthesis files from this
directory into `hdl/fpga/ip/nuand/rf_observatory/` of that copy:

- `rf_observatory_p.vhd`
- `rf_channel_bank.vhd`
- `rf_channel_framer.vhd`

The patch adapts the micro hosted QIP/top level. It selects the single enabled
physical ADC source, sends framed words through logical stream zero, and retains
the upstream sample FIFO, FX3 and RFIC control paths. Zero or two ADC inputs enabled,
packed/8-bit samples, native metadata, packet mode, alternate RX mux selection or
armed RX triggering are unsupported and reported as configuration errors.
The wrapper forces its downstream transport to plain 16-bit, metadata-free,
normal-mux sample mode. The upstream transmit datapath/control remains present.

The bank and serializer use the existing AD9361 RX clock domain. The trigger-arm
diagnostic originates in the Nios clock domain and passes through the existing
Nuand synchronizer before entering the RX-domain sticky configuration flag.
Their reset is `rx_reset OR NOT rx_enable`, using the upstream RX-enable
synchronization. Disabling reception
therefore clears the numerical state, pending record, source index and sticky
status before a subsequent receive epoch. Downstream FIFO reset remains under
the upstream FX3/RX-enable logic. This relationship is source-level integration
evidence, not fitted clock timing or a measured restart-alignment guarantee.

The framed words do not carry measured sample rate or retune notifications.
Keep the physical source at 20 MSps and preserve its actual readback externally.
Stop reception and start a new capture file when changing sample rate, RF
center, physical input or other interpretation-critical settings. In particular,
a nominal profile ID and an increasing accepted-sample index cannot establish
that the RFIC remained continuously tuned throughout an acquisition.

Treat the resulting image as a custom channel-bank image even though the
isolated vendor build still uses its `hosted` target name. Preserve its source
revision, patch/module hashes and build reports alongside the output image;
do not substitute it silently for a stock hosted image.

## Quartus baseline and custom image build

Use **Quartus Prime Lite 23.1.1 for Linux**, with **Cyclone V device support**
and the matching **Nios II Embedded Design Suite**, as recommended by the
[pinned Nuand FPGA development wiki](../../../repos/bladeRF-wiki/FPGA-Development.mediawiki).
The [pinned source's original HDL guide](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/hdl/README.md#required-software)
specified **20.1.1**; this workflow selects 23.1.1 and first reproduces the
unmodified pinned design. Select the free Nios II/e `tiny` configuration below.
Native host-build prerequisites are described in the [setup guide](../../../docs/SETUP.md).

Verify that the Platform Designer catalog can generate **`altera_nios2_gen2`**
before starting a fresh source build. Altera's
[Nios II preservation procedure (AN 1009, section 3)](https://docs.altera.com/r/docs/839394/current/an-1009-preserving-nios-ii-processor-ip-for-quartus-prime-version-24.1-and-newer/preserving-nios-ii-processor-ip-in-quartus-prime-standard-edition-version-24.1std-or-newer?contentId=HEj64Ii9GIhuvwmVVE_oEQ)
describes reusing previously generated IP with Quartus Standard 24.1std and
newer. Use `doctor` to check executable availability and inspect the IP catalog
separately.

First prepare an unmodified source clone and generate/build its host-side
dependencies. Run this block from the workspace root, using a fresh destination:

```sh
export RF_OBSERVATORY_ROOT="$PWD"
export RF_STOCK_SOURCE="$RF_OBSERVATORY_ROOT/build/rf-observatory/stock-source"
mkdir -p "$RF_OBSERVATORY_ROOT/build/rf-observatory"
git clone --no-hardlinks "$RF_OBSERVATORY_ROOT/repos/bladeRF" "$RF_STOCK_SOURCE"
git -C "$RF_STOCK_SOURCE" checkout --detach fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47
git -C "$RF_STOCK_SOURCE" submodule update --init --recursive
cmake -S "$RF_STOCK_SOURCE/host" -B "$RF_STOCK_SOURCE/host/build"
cmake --build "$RF_STOCK_SOURCE/host/build" --parallel 2
```

Set the installed SDK root, then enter its Nios II command shell. The example
path is a placeholder for the directory containing `quartus/` and `nios2eds/`:

```sh
export RF_QUARTUS_INSTALL=/path/to/intelFPGA_lite/23.1std
export PATH="$RF_QUARTUS_INSTALL/quartus/bin:$RF_QUARTUS_INSTALL/quartus/sopc_builder/bin:$PATH"
"$RF_QUARTUS_INSTALL/nios2eds/nios2_command_shell.sh"
```

In that shell, verify the selected compiler and tools, then build the stock A9
image before applying any custom logic:

```sh
quartus_sh --version
command -v qsys-script qsys-generate nios2-bsp-create-settings
cd "$RF_STOCK_SOURCE/hdl/quartus"
./build_bladerf.sh -b bladeRF-micro -s A9 -r hosted -n tiny -l full -S 1 -D
```

On Linux, a `NativeTcl` or ICU loader failure during BSP generation can require
the SDK's `quartus/linux64` libraries. Scope that library path to
`nios2-bsp-create-settings` and BSP compilation; retain the upstream
[BSP arguments](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/hdl/quartus/build_bladerf.sh#L445-L473).
Then resume the wrapper with the normal host environment and its existing
generated BSP settings. Exporting the vendor directory globally can break host
CMake through an older C++ runtime; putting system libraries first can instead
select an incompatible Tcl runtime. Check the actual BSP output and subsequent
build stages, including the RAM initialization files, before accepting success.

The stock source, submodule revisions and generated output remain isolated from
`repos/bladeRF`. The fixed work output is under
`hdl/quartus/work/bladerf-micro-A9-hosted/output_files/` in that clone, with
`hosted.rbf` and the synthesis, fitter and timing reports. Nuand's script also
creates a timestamped output bundle. Review resource utilization, setup/hold
slack, clocks and unconstrained paths; successful assembly alone does not
establish acceptable timing. Retain this stock build as the baseline.

Prepare a second fresh clone for the channel bank. These commands run inside
the same Nios II shell, retaining the exported workspace variable:

```sh
export RF_CUSTOM_SOURCE="$RF_OBSERVATORY_ROOT/build/rf-observatory/channel-bank-source"
git clone --no-hardlinks "$RF_OBSERVATORY_ROOT/repos/bladeRF" "$RF_CUSTOM_SOURCE"
git -C "$RF_CUSTOM_SOURCE" checkout --detach fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47
git -C "$RF_CUSTOM_SOURCE" submodule update --init --recursive
git -C "$RF_CUSTOM_SOURCE" apply --check "$RF_OBSERVATORY_ROOT/patches/bladerf-rf-observatory-channel-bank.patch"
git -C "$RF_CUSTOM_SOURCE" apply "$RF_OBSERVATORY_ROOT/patches/bladerf-rf-observatory-channel-bank.patch"
mkdir -p "$RF_CUSTOM_SOURCE/hdl/fpga/ip/nuand/rf_observatory"
cp "$RF_OBSERVATORY_ROOT/projects/rf-observatory/hdl/rf_observatory_p.vhd" "$RF_CUSTOM_SOURCE/hdl/fpga/ip/nuand/rf_observatory/"
cp "$RF_OBSERVATORY_ROOT/projects/rf-observatory/hdl/rf_channel_bank.vhd" "$RF_CUSTOM_SOURCE/hdl/fpga/ip/nuand/rf_observatory/"
cp "$RF_OBSERVATORY_ROOT/projects/rf-observatory/hdl/rf_channel_framer.vhd" "$RF_CUSTOM_SOURCE/hdl/fpga/ip/nuand/rf_observatory/"
cmake -S "$RF_CUSTOM_SOURCE/host" -B "$RF_CUSTOM_SOURCE/host/build"
cmake --build "$RF_CUSTOM_SOURCE/host/build" --parallel 2
cd "$RF_CUSTOM_SOURCE/hdl/quartus"
./build_bladerf.sh -b bladeRF-micro -s A9 -r hosted -n tiny -l full -S 1 -D
```

The custom image is this second clone's
`hdl/quartus/work/bladerf-micro-A9-hosted/output_files/hosted.rbf`. Keep its
location distinct from the stock image, review its fitted timing and preserve
the compiler version, source/patch/module hashes and reports. The seed and
date-omission options help specify the build inputs; they do not promise
identical bitstreams across toolchain changes.

For the guarded host wrapper, package the renamed RBF with a sibling schema-1
manifest only after fitter and TimeQuest review. The project guide's
[manifest contract](guide.md#guarded-profile-1-ram-capture) lists the required
compiler, seed, source hashes, complete timing results and artifact identity.
Keep that package and its supporting reports in private build storage.

These commands describe the board build path. The portable RTL verification
does not replace a successful Quartus fit or a loaded-image test. The custom
image's hardware clocking, native transport and receive performance remain
unqualified until those checks are performed.

## Dedicated native framed-word capture

The project guide's guarded `capture-fpga` command automates this procedure for
the fixed profile-1 contract, including input hashes, bounded capture, complete
frame validation and immediate stock restoration. This section remains the
lower-level operational and recovery reference. Use it when qualifying a new
image or diagnosing a wrapper failure; preserve the same fresh-file, RAM-only,
single-RX and restoration requirements.

Use the pinned `bladeRF-cli` with plain **16-bit binary** RX capture on a
little-endian host. This preserves transport-header words outside the Q11
component range: the pinned
[`sc16q11_sample_fixup` and binary writer](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/utilities/bladeRF-cli/src/cmd/rx.c#L39-L95)
perform endian conversion and write `int16_t` components; their former Q11
masking code is commented out. The plain
[`BLADERF_FORMAT_SC16_Q11` receive path](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/streaming/sync.c#L645-L673)
copies bytes without the separate packed-format unpacking. On a big-endian
host the CLI writes host-order words, which must be converted before using the
little-endian decoder.

After a successful custom build and timing review, create a fresh private
capture directory from the workspace root. Load the explicit custom image
path for this session; the following is a hardware-use procedure, not evidence
that the image has already passed a board test:

```sh
cd "$RF_OBSERVATORY_ROOT"
mkdir -p captures
mkdir -m 700 captures/fpga-profile1-session
"$RF_STOCK_SOURCE/host/build/output/bladeRF-cli" -i -l "$RF_CUSTOM_SOURCE/hdl/quartus/work/bladerf-micro-A9-hosted/output_files/hosted.rbf"
```

On Linux, `A memory allocation error occurred` during image loading can mean
the shared usbfs buffer quota is too small. The loader sends the complete image
in [one bulk transfer](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/backend/usb/usb.c#L411-L448).
Check the current ceiling and, if needed, increase it for the current boot:

```sh
cat /sys/module/usbcore/parameters/usbfs_memory_mb
printf '64\n' | sudo tee /sys/module/usbcore/parameters/usbfs_memory_mb
```

This changes the allocation ceiling rather than reserving that much RAM; see
the [kernel parameter documentation](https://www.kernel.org/doc/html/latest/admin-guide/kernel-parameters.html).
The loader begins FPGA programming before the transfer, so a failed load can
leave the FPGA unconfigured. Resolve the transfer error and reload a known-good
stock image before retrying the custom session.

At the bladeRF CLI prompt, the following example selects **RX1** and a 98 MHz
RF center. Read back the actual physical settings before starting:

```text
set bitmode 16
set rx_mux BASEBAND
set loopback none
set biastee rx1 off
trigger J51-1 rx off
set samplerate rx1 20000000
set bandwidth rx1 20000000
set frequency rx1 98000000
set agc rx1 off
set gain rx1 40
print samplerate rx1
print bandwidth rx1
print frequency rx1
print gain rx1
rx config channel=1 format=bin file=captures/fpga-profile1-session/channel-bank.frames n=8000000 samples=16384 buffers=128 xfers=32 timeout=5s
rx start
rx wait 10s
rx
```

The CLI [warns that gain changes may be deferred until the receive channel is enabled](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/utilities/bladeRF-cli/src/cmd/printset_impl.c#L787-L830).
Its pre-start gain readback can therefore describe the previous setting; retain
the warning and check gain while reception is enabled when the applied value
matters. A requested gain alone is not an applied-gain measurement.

For **RX2**, change each `rx1` setting to `rx2` and use `channel=2`; do not
enable `channel=1,2`. RX1 records have source bit 5 clear; RX2 records have it
set. The FPGA's four output channels are DSP branches of that one selected
physical input. Plain CLI bitmode 16 selects metadata-free SC16 transport;
there is no additional native metadata or packet mode in this capture command.
Do not use 8-bit or packed formats for these header words.

`n` counts **complex transport slots**, not physical RF samples. Exactly
8,000,000 slots contain 1,000,000 complete records and occupy 32,000,000 bytes,
nominally one second of 20 MSps source input. Use the exact decimal count:
the CLI's `M` suffix means a multiple of 1024², so `8M` is a different count.
Initialization and stream delivery add wall time. An `rx wait` timeout requires
checking the RX task and stopping it; it does not certify a completed capture.
The CLI can truncate an existing output file, so use a fresh session directory
and file for each attempt.

After RX is idle and reports no native error, leave the CLI and decode the raw
records with the project's dedicated `decode-fpga` command, as shown in the
[project guide](../README.md). Preserve the CLI log, file length, selected input,
actual tuning/rate settings and source/build identifiers with the capture.
The decoder rejects malformed headers, unsupported profiles, clipping/loss/
configuration flags, source changes and index discontinuities. Parsed framing
alone still does not certify the absence of RF or downstream loss.

Before using ordinary hosted-IQ applications again, reload the stock image from
the baseline build and check the reported version:

```sh
"$RF_STOCK_SOURCE/host/build/output/bladeRF-cli" -l "$RF_STOCK_SOURCE/hdl/quartus/work/bladerf-micro-A9-hosted/output_files/hosted.rbf" -e version
```

`-l` loads FPGA RAM for the session. `-L` writes flash and is not part of this
procedure. The normal version string alone does not distinguish these custom
images; preserve the loaded file's hash and source inputs with each session.

## Verification and its limits

The numerical testbench reads the same input samples and oscillator words as
the Rust model and writes every output group for exact comparison. It inserts
input gaps and long output stalls, asserting output stability and index order.
The pipeline-control testbench checks a 19-sample partial group, a complete
20-sample group draining without further input, reset at each pending stage,
and output backpressure with later samples still in flight. Its phase-zero
fixtures use an independent three-boxcar FIR response to check numerical output
after stalls and reset.
The framed-word testbench checks the fixed profile's serialized output under
short output stalls. The contract testbench exercises pending-record reset,
ADC loss, source changes, FIFO overflow and configuration flags.

`rf_channel_trigger_cdc_tb` drives off-clock trigger-arm changes through the
pinned Nuand synchronizer into the actual framer, checking delayed arrival,
sticky configuration status, disarm and a clean reset epoch. It uses the same
connection as the integration patch, but does not elaborate the complete board
top level. Source review and fitted CDC/timing assessment remain necessary;
digital simulation cannot model physical metastability.

`rf_channel_fifo_tb` instantiates the pinned Nuand `fifo_writer`, with one logical
stream enabled in its two-stream packer, and verifies each 64-bit FIFO write
against two consecutive framed slots. Its upstream `synchronizer` has a
port-attribute placement requiring GHDL's `-frelaxed-rules`; retain that narrow
compatibility option for the vendor test. Authored synthesis sources use strict
VHDL-2008 analysis and GHDL synthesis.

`rf_wishbone_master_contract_tb` instantiates the exact pinned Nuand
Avalon-to-Wishbone master. The verifier separately freezes its Qsys word-address
units and 24-bit exported address width, then checks byte-to-word examples,
serialized reads and writes, held commands, delayed ACKs and terminating unknown
addresses. The pinned handshake is unusual: `cyc` plus constant-high `stb` is
the brief request-capture window, while `sel` is the later response-wait state.
A slave must latch address, data and write-enable during `cyc`, then ACK exactly
once from that saved request only after `sel` is active.

Both bridge clocks use one simulation clock because the pinned process
sensitivity and sampled clock differ. The simulation-only show-ahead FIFO model
invalidates popped data but does not model Intel FIFO CDC, timing or
metastability. A second direct bench qualifies the standalone profile-2 register
contract, and an integration bench connects that slave to the exact pinned
master while directly driving master/slave reset with bounded skew. The register
slave also passes generic synthesis. Separate profile-2 benches exercise three
unrelated control/RX clock/phase matrices through the pinned Nuand synchronizer,
check the four-phase level handshake and 160-bit held payload, and integrate the
real register slave with RX-active rejection and the one-start-token lifecycle.
The standalone control CDC also passes generic synthesis.

The profile-2 file bench drives the real channel bank with the frozen epoch and
phase words, requires 20 leading zero inputs for an independent first-DATA
oracle, inserts output stalls and writes every CONFIG/DATA slot. Before the
production parser is used, the verifier byte-compares the complete CONFIG and
first index-19 DATA records with a literal 64-byte vector. It then strictly
parses the complete stream and compares every DATA payload with the Rust fixed-
point model. A separate lifecycle bench covers CONFIG and DATA stalls, admission
gating, invalid-start diagnostics, CONFIG-time fail-closed behavior, sticky
diagnostics, resets of partial output and restart at index 19. The framer also
passes generic synthesis. Pure Rust tests use a separate nonzero literal DATA
vector and exercise malformed headers, rates, flags, order, tags, indexes,
overflow and partial records without calling an encoder.

The maintained verifier also exercises the device-independent host controller
against a bounded CPU register model, then reconciles its requested and applied
state with a synthetic RFIC readback fixture and the strict parsed stream. A
CONFIG-only diagnostic cannot reach reconciliation because strict validation
requires at least one clean DATA record. The result exposes
`profile2_host_control_contract_passed`; this CPU check shares the transport
comparison and does not add an external GHDL stage, so the count remains 54.

`rf_profile2_init_reset_tb` checks fail-closed PLL-lock handling, the 16-cycle
system hold and two-stage RX release. `rf_profile2_slot_packer_tb` checks FIFO-
full backpressure, same-event second-slot/write behavior, exact halfword order
and partial-pair reset. `rf_profile2_integration_tb` connects the real register,
CDC, framer, bank and packer; it proves that an ordinary pre-enable stream reset
preserves a stopped commit, CONFIG occupies the first four writes before DATA
admission, a blocked partial DATA record is discarded on stop, and restart begins
with a new exact CONFIG. All three synthesizable entities also pass generic GHDL
synthesis. The maintained command comprises 54 stages and exposes three separate
success fields for init reset, slot packing and the FIFO-write-boundary lifecycle.

The maintained GHDL stages exercise the standalone entities and reference
models. The exact-FIFO model checks FIFO behavior. The full Quartus build
elaborates the generated reset controller and target top, then reports fitted
synchronizer placement and all-corner timing. Follow the
[project guide](../README.md) for the host and hardware verification procedures.

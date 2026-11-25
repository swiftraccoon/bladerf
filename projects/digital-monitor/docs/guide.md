# Digital-signal monitor

`digital-monitor` surveys SC16_Q11 recordings and runs the workspace's passive
decoders. Use `analyze` for a saved file, `watch` for finite RX1 snapshots, or
`watch-segments` for rolling RX1 monitoring. First prepare the
[Wi-Fi decoder and native dependencies](../../wifi-monitor/docs/guide.md#build-and-check),
including the ordered libcrafter patches. Build the tools from the repository root.

```console
cargo build --locked --release -p digital-monitor -p bladerf-lab -p wifi-monitor -p rf-observatory -p ieee802154-monitor -p bluetooth-monitor
```

Build the
[FutureSDR file worker](../../ieee802154-monitor/futuresdr-worker/docs/guide.md#build-and-run)
in release mode from its own directory and build the
[Blue Dragon native decoder](../../bluetooth-monitor/docs/guide.md#build-the-native-engines)
before running the corresponding stages.

After exporting verified IQ slices with [rf-observatory](../../rf-observatory/README.md),
compare their coverage against a trusted, closed analysis of the same capture:

```console
target/release/digital-monitor compare-event-replay captures/session/iq.sc16q11 \
  --analysis artifacts/digital-monitor/session --slices artifacts/slices \
  --protocol wifi --output artifacts/slice-comparison
```

Use `wifi` or `bluetooth`. The command verifies slice ranges and compares replay
with the reference decoder build. Use
`qualify-event-replay INPUT --analysis DIR --output DIR` to survey up to 20
million complex samples, export active-window IQ once, and compare both lanes.
`--target-slices 16` limits launches per lane by bridging the smallest gaps,
which increases the amount of IQ replayed. `--overlap-events` enables half-window
overlap for the survey.

Both commands accept `--jobs 1` through `4` and `--timings`. Qualification's
`--overlap-lanes` shares the job limit across both lanes. Reported decoder
times sum across jobs, while replay time measures elapsed wall time. Packet,
output-byte and time limits are checked between job groups, so a failure can
leave partial artifacts.

## Segmented RX1 monitoring

`watch-segments` surveys each RX1 segment with 2,440 FFT windows. Its event
detector covers the nominal 20 MHz sampled band outside the ±100 kHz center
guard and reports relative power.

```console
target/release/digital-monitor watch-segments --center-mhz 2437 \
  --segments 10 --decode-limit 1 --output captures/digital-segments
```

### Run length and retention

- Without spooling, a run keeps up to 12 segments and fully analyzes up to
  `--decode-limit` of them after capture. Selection is chronological unless
  `--decode-selection activity` ranks by active windows, with earlier segments
  winning ties. Full analysis requires the
  [IEEE channel coverage](#finite-rx1-monitoring) below. Set `--decode-limit 0`
  to skip it.
- `--survey-workers` accepts 1–6, default 1. `run.json` records survey timing,
  selected segment indices, stage counts and terminal state in capture order.
- `--spool-segments N` sets 1–120 raw slots of about 80 MB each and permits up
  to 120 segments. The spool releases IQ once the required receipts are saved
  and keeps full-decode selections. A full spool stops capture.
- `--writer-queue-depth N` lets a compatible
  [receiver](../../../tools/bladerf-lab/README.md) queue 1–8 completed segments in
  memory before disk writes. It is independent of the disk spool limit.
- `--usb-buffer-samples N` requests the receiver's USB transfer size in complex
  samples. Use a multiple of 2,048 from 2,048 to 65,536. `run.json` records the
  value when supplied.
- With spooling and `--decode-limit 0`, `--keep-evidence-segments N` permits
  up to 3,600 segments. It retains the latest N completed segments'
  evidence and checkpoints earlier verified counts, clock bounds and report
  hashes in `retired.json`. A cleanup failure stops the run and can leave
  extra evidence.
- With bounded evidence retention, use `--until-finished` in place of
  `--segments`. Create an empty `finish` file in the output directory to
  finish at a verified boundary and drain decoders. Capture fails after 30
  seconds without a new segment.
- `--recoverable-restarts N` allows N new epochs after overflow, receive
  timeout, timestamp gap or spool pressure, after verifying released IQ.
  The output contains `epochs/`, the final `session.json`, and the
  session-level `finish` path.
- `--short-epoch-restarts N` also limits consecutive restarts after epochs
  that released fewer than 30 verified segments. An epoch reaching 30
  clears this streak. The total restart limit still applies.

### Direct decoding

- `--decode-wifi-ble` decodes all surveyed segments during capture and binds
  decoder receipts before spool release. `--blue-dragon PATH` selects a
  compatible prebuilt BLE decoder.
- `--wifi-channel-center-mhz` accepts a center within 2 MHz of capture
  tuning. Shifts over 1 MHz use anti-wrap filtering. The receipt records
  both centers, the signed shift and filter. Packet spans use capture
  sample coordinates. `--wifi-double-q11` doubles Q11 components before
  decoding and reports clipped components, including overlap.
- `--decode-boundaries` uses adjacent IQ for Wi-Fi and BLE packets crossing
  a segment boundary. Receipts identify the interval that owns each packet
  start. Each nonfinal segment keeps a 20.48 ms tail. Extended inputs use
  temporary storage outside the spool.
- `--track-wifi-ht-modes` counts owned HT preambles and exact FCS-valid
  Wi-Fi frame-start matches by mode. The verified `wifi-ht-modes.json`
  sidecars roll into `retired.json` and the terminal `run.json`.
  `limited_segments` counts segments with header diagnostic capacity limits.
- `--decode-ieee802154` runs hard decoding and independent FCS checks for
  channels 16–18 at capture centers of 2433–2437 MHz. The spool retains
  archives and decoder receipts. Channelized IQ uses temporary storage.
  `--decode-ieee802154-edge` adds channel 19 at 2437 MHz, where filter coverage
  reaches the sampled edge. Compatible workers accept
  `--ieee-executor-threads` from 1 to 8 per process.
- With `--decode-ieee802154`, use `--decode-ieee802154-boundaries` to replay
  the contiguous predecessor's 20.48 ms tail. The
  [indexed hard worker](../../ieee802154-monitor/futuresdr-worker/docs/guide.md#candidate-source-positions)
  lets the inspector assign each candidate to the segment containing the
  input sample needed to finish it. Packets completed in the prefix are
  excluded from that segment's validated count.

With an IQ spool, `--coded-sentinel-every 60 --coded-sentinel-decoder PATH`
offers a low-priority replay every 60 release opportunities. The
[selected Coded scan](../../bluetooth-monitor/docs/guide.md#selected-le-coded-scan)
rotates through interior BLE data bins, carrying its cadence and rotation
across receiver recovery epochs. It skips an opportunity when busy or when
pending IQ exceeds one quarter of the spool slots, rounded up. Set
`--coded-sentinel-wrapper PATH` to select a separate replay executable.
With a compatible wrapper and decoder, `--coded-sentinel-omit-squelch-iq`
keeps selected-bin packet decoding while avoiding the ordinary IQ archive cap.
Each epoch's `coded-sentinel.json` records coverage, CRC-valid packet counts
and the latest failure with bounded log excerpts. `coded-sentinel.pcap` keeps
validated packets from the latest positive replay. The capture uses its usual
release checks.

### Retained clips and burst measurements

`--retain-event-clips 2` saves up to two full-band clips per segment, each up
to 960 KiB outside the IQ spool. Receipts bind them to source sample ranges.

- `--event-clip-selection mixed` reserves a slot for a burst up to 400 kHz
  wide and two to ten survey windows long. `contrast` adds a 3 dB adjacent
  band margin, and `narrow-search` also considers independently detected
  bursts. `--probe-event-clips` checks eligible clips for symbol timing.
- `--wifi-frame-context` requires direct Wi-Fi/BLE decoding and favors
  clips outside validated Wi-Fi spans. With it, `--retain-ht-signal-clip`
  can keep IQ around a checked HT header outside those spans if the full
  clip fits.
- `--retain-he-su-signal-clip` uses the same Wi-Fi context and requires a
  `wifi-monitor` build with
  [HE20 header events](../../wifi-monitor/docs/guide.md#replay-event-sidecars).
  Counts follow header completion across overlap. Full HE clips use an
  available slot before HT and short-burst clips. The first complete HE
  clip in each epoch survives retirement in `first-he-su-clip/`.
  With `--wifi-he-decoder PATH`, a low-priority worker replays that archive
  using the [pinned HE receiver](../../wifi-monitor/docs/guide.md#he20-single-user-payload-replay).
  `he-su-payload.json` records its status and `first-he-su-payload/` holds
  verified output after replay completes.
- `--burst-band-hz LOWER:UPPER` measures relative power on a 51.2 µs grid.
  Repeat `--burst-context-band-hz` for up to seven disjoint companion bands.
  `--retain-band-burst-clip` reserves the strongest eligible burst of at
  least four windows. `--retain-short-band-burst-clip` uses an available
  slot after long-burst and Wi-Fi header reservations for a two- or three-window
  burst at least 10 dB above baseline.
- `--track-short-burst-cadence` scores 1 ms recurrence using the prior 32
  segments in the same epoch. It requires a burst band and direct decoding,
  excludes validated Wi-Fi spans, and reports exposure and abstentions.

## Finite RX1 monitoring

`watch` repeats RX1 snapshots at 20 MSps and 20 MHz bandwidth. The defaults
are one second and 2437 MHz. With `--duration 2`, use `--windows 256` to keep
the default survey-window density. A two-second run needs 160 MB of raw IQ
and 192 MB of channelized IQ per retained session, before other artifacts.

```console
target/release/digital-monitor watch \
  --cycles 10 --center-mhz 2437 --gain 40 --keep-sessions 3 \
  --max-consecutive-failures 3 --retained-limit-mib 1024 \
  --output captures/digital-watch
```

Use one attached bladeRF throughout the run with stock FPGA `0.16.0`. The
watch selects RX1, disables bias tee and loopback, and validates realized
settings, IQ, sidecars and capture logs. See the
[receive setup guide](../../../docs/SETUP.md#validation).

- `--center-mhz` accepts integers from 2433 to 2437 MHz, leaving edge margin
  for IEEE 802.15.4 channels 16–18. `--alternate-center-mhz` accepts a
  different center in that range and requires at least two cycles and two
  retained-session slots. Odd attempts use the primary center and even
  attempts use the alternate, including failed attempts. Each snapshot
  nominally covers ±10 MHz. There are gaps between captures and coverage at
  the sampled edges is uncalibrated.
- `--pending-captures 1` overlaps one validated capture's analysis with the
  next capture. The default is sequential. Pending IQ counts toward storage
  limits and is protected from pruning. On interruption or worker failure,
  any capture underway can finish and its IQ is kept.
- `--discard-initial-samples` omits up to 20 million complex samples before
  saving each snapshot. Include the discarded interval in the capture
  timeout. The sidecar and `run.json` record the verified count.
- `--bluetooth-coded-shadow` adds file-only LE Coded replay of the 2426 MHz
  primary path for 2435 MHz captures with 327,680–20,000,000 samples. Use
  snapshots of at most one second and include 2435 MHz in the schedule. Other
  centers report `not_eligible`. Evidence in `bluetooth-coded-shadow/` counts
  toward retention limits. A replay failure makes the attempt incomplete.

Choose 1–1,000 cycles. Failures consume their cycle and preserve evidence. A
full success resets the failure streak. The watch stops at the configured
consecutive-failure limit or on Ctrl-C.

### Watch reports

The watch syncs transitions to `attempts.jsonl` before atomically updating
`run.json`. The report binds source and manifest hashes, stage outcomes and
program identities. A program identity change stops the watch. A source, stage
or summary mismatch fails the attempt and preserves the session.

Run schemas are 2 for sequential and 3 for overlapping single-center watches,
4 for mixed-center watches, and 5 when the Coded shadow is enabled. All retain
schema-14 metrics and source summary hashes before pruning. The aggregates
cover [Wi-Fi packets](../../wifi-monitor/docs/guide.md#what-can-be-observed) and
[rejections](../../wifi-monitor/docs/guide.md#outputs-and-failure-handling),
[RF activity](../../rf-observatory/docs/guide.md#record-and-inspect-a-wideband-window),
[IEEE 802.15.4 hypotheses](../../ieee802154-monitor/docs/guide.md#compare-carrier-offset-hypotheses),
and [Bluetooth and waveform families](../../bluetooth-monitor/docs/guide.md#capture-and-inspect).
One waveform group can contribute to several frequency bins. IEEE totals count
distinct decoder outputs, which can differ in completion position for the same
physical frame. Compact metrics omit device serials, decoded identifiers,
timestamps, raw IQ and payloads.

### IEEE 802.15.4 resource bounds

Each of three channel workers inspects hard then soft candidates at each
of 0, −100,000 and +100,000 Hz and compares the six sources in that order.
The bank tests only those offsets. A complete analysis runs 40 children, up
to three at once, with a default 180-second deadline per child. Budget up to
1,152 MiB for the 18 candidate archives and 9 GiB for inspector outputs,
plus channelized IQ and FutureSDR temporary buffers. The private IEEE
analysis directory is used as `TMPDIR`. Each channel's union is limited to
six million identities. Input and output identities are rechecked before
publishing results.

### Session retention

The watch prunes successes only after saving their compact results. Routine
successes are pruned first. Priority goes to Bluetooth or IEEE 802.15.4
candidates, narrow binary-FSK-like groups, Wi-Fi OFDM DATA failures, complete
OFDM decodes with bad FCS, and validated Coded shadow packets. Within a
priority, the oldest eligible session goes first. Failures, interruptions
and the sole successful raw session at each center are protected. The watch
stops if the byte limit cannot accommodate that minimum. An active cycle can
exceed the limit before the next check.

`run.json` records retention priorities and reasons, and `attempts.jsonl`
journals pruning before deletion. A completed plan can exit with failure if
any attempt failed. Use a fresh output path.

## Inventory recurring waveform-family bins

`inventory-recurrence` is a host-only follow-up for one terminal schema-2,
schema-3, schema-4 or schema-5 `run.json`. It reads no IQ, burst archive or retained
session and owns no radio, network or transmit interface:

```console
target/release/digital-monitor inventory-recurrence \
  captures/digital-watch/run.json \
  --output artifacts/digital-monitor/recurrence
```

The input is limited to 16 MiB and 1,000 terminal attempts. Completed
analyses must have valid compact metrics schema 11–14. Each family and MHz
bin gets at most one vote per eligible capture. A bin recurs when support
reaches `max(3, ceil(eligible_captures / 5))`.

`recurrence.json` binds the input hash and reports support, streaks and count
distributions. It uses schema 1 for single-center runs and schema 3 for
mixed-center runs. Mixed-center results keep separate thresholds and counts
per center and match recurring family labels by absolute bin or
center-relative offset. This measures recurrence of provisional shape labels.

`compare-recurrence` compares two mixed-center schema-3 inventories without
opening their source runs or any device:

```console
target/release/digital-monitor compare-recurrence \
  artifacts/digital-monitor/baseline/recurrence.json \
  artifacts/digital-monitor/current/recurrence.json \
  --output artifacts/digital-monitor/comparison
```

Both plans must be fully successful and have matching center schedules,
family order and nominal bands. `comparison.json` distinguishes recurring,
observed nonrecurring and absent rows at each center. Each inventory and
comparison report is limited to 1 MiB and published atomically in a fresh
private directory.

## Analyze an existing capture

```console
target/release/digital-monitor analyze captures/session/iq.sc16q11 \
  --output artifacts/digital-monitor/session
```

Use UTF-8 paths and a regular little-endian SC16_Q11 file with an adjacent
`INPUT.json` acquisition sidecar. The default input is centered at 2437 MHz
with 20 MSps and 20 MHz bandwidth. Provide up to 20 million complex samples
in multiples of ten. Sidecar settings, sample count, byte extent and each
Q11 component are checked before replay. Use prebuilt decoders and a fresh
output directory.

Wi-Fi replay and FCS inspection, RF survey, IEEE 802.15.4 processing, and
Bluetooth replay run concurrently. Burst-family classification uses the
retained Bluetooth burst archive. See the
[IEEE worker limits](#ieee-802154-resource-bounds) and
[Bluetooth classifier guide](../../bluetooth-monitor/docs/guide.md#capture-and-inspect).
The supervisor checks complete Bluetooth channelizer input accounting and
revalidates input, sidecar and executable identities.

`analysis.json` schema 2 records stage states and outcomes. Successful stages
join both log-drain workers. Failures preserve partial outputs and return
nonzero. Timeout or Ctrl-C gives decoder process groups five seconds before
forced cleanup. Both log drains then share a five-second deadline. If that
deadline expires, the stage fails and requests cancellation. A pending
filesystem operation can outlast cancellation.

`evidence_found` indicates independently validated frame bytes.
`unclassified_activity` can include analog energy, noise or receiver
artifacts. Burst-family labels describe only retained, classified groups.
The RF survey covers the central 16 MHz outside the ±100 kHz guard. IEEE
802.15.4 decoding covers supported 2.4 GHz O-QPSK on channels 16–18.
The default 2437 MHz capture excludes the Bluetooth primary advertising
centers. See the [Wi-Fi PHY coverage](../../wifi-monitor/docs/guide.md#decoder-coverage).

(rates-formats-and-limits)=
# Distinguish tuning, sample rate, bandwidth, and format

**Purpose:** interpret common bladeRF specifications without mixing quantities
or discarding their conditions. Source-code limits on this page refer to
Nuand's `2025.10` release. They describe that implementation, not a measurement
of sustained streaming on a particular computer.

## Ask which quantity a number describes

| Quantity | Meaning | Common unit |
| --- | --- | --- |
| RF tuning frequency | Where the analog RF chain is centered | Hz |
| Complex sample rate | Number of I/Q sample pairs per second in one channel | samples/s |
| Analog filter bandwidth | Bandwidth configured in the RF signal path | Hz |
| Host sample representation | Memory/file layout of those samples | bytes per complex sample |
| Stream throughput | Data moved per time, with a specified channel count and format | bytes/s |

Frequency, filter bandwidth, and sample rate can all appear as a value containing
“mega.” They remain different controls. In particular, doubling sample rate
does not establish twice the usable analog bandwidth or guaranteed USB
throughput. Types and documentation should retain those separate meanings.

## RX and TX do not have the same minimum tuning frequency

The pinned implementation defines these ranges:

| Direction | Minimum | Maximum |
| --- | ---: | ---: |
| RX | 70,000,000 Hz | 6,000,000,000 Hz |
| TX | 47,000,000 Hz | 6,000,000,000 Hz |

These values come from `bladerf2_rx_frequency_range` and
`bladerf2_tx_frequency_range` in the
[shared micro definitions](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/fpga_common/include/bladerf2_common.h#L549-L563).
The board implementation selects a range by direction in
[`bladerf2_get_frequency_range`](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L1232-L1245).

Nuand's product headline uses the broader 47 MHz–6 GHz description. Its
[detailed specification table](https://www.nuand.com/bladerf-2-0-micro/)
distinguishes RX from TX. Software that stores one universal
`MIN_FREQUENCY = 47 MHz` loses this distinction. Query the active device and
direction when validating a requested configuration; keep the static table
qualified by the source release.

## Separate the ordinary and extended sample-rate modes

| Source-defined mode | Sample-rate range | Qualification |
| --- | --- | --- |
| Base | 520,834–61,440,000 samples/s | Source-defined base-mode range |
| Oversample feature | 6,250,000–122,880,000 samples/s | Explicit extended-mode feature |

The same
[source definitions](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/fpga_common/include/bladerf2_common.h#L507-L546)
contain both ranges. The
[sample-rate range query](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L979-L992)
selects the range according to the enabled feature. These ranges are not a
promise that every host, channel layout, stream format, and application can
sustain their maximum.

Nuand's [2023.02 technical announcement](https://www.nuand.com/2023-02-release-122-88mhz-bandwidth/)
describes the extended mode as an AD9361 overclock/filter-bypass feature and
explains the accompanying 8-bit transport support. The ordinary advertised
rate and this extended mode must have separate entries in a reference or
capability model. The pinned
[bandwidth setter](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L1210-L1230)
also warns about bandwidth assignments while oversample is enabled. Do not
promote extended-mode settings into unconditional operating guarantees.

## Understand SC16 Q11 on the host

For `BLADERF_FORMAT_SC16_Q11`, one complex sample contains an I component
followed by a Q component. Each component occupies a signed, little-endian
16-bit integer. The documented component range is −2048 through 2047;
division by 2048 maps it to approximately −1 through +1, with the upper endpoint
excluded. The name describes a fixed-point representation, not 16 bits of ADC
resolution. See the
[versioned sample-format definition](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/include/libbladeRF.h#L2085-L2142).

For a **single channel**, with no in-band metadata or file header:

```text
bytes per complex sample = 2 components × 2 bytes = 4 bytes
file bytes = complex sample count × 4
host payload rate = complex sample rate × 4
```

For example, 2,000,000 complex samples occupy 8,000,000 bytes. At an actual
sample rate of 2,000,000 samples/s, those samples span one second of sample
time. This is arithmetic from the representation; it is not a measurement of
elapsed capture time or proof that no samples were dropped.

In a two-channel layout, the documented order alternates a complex sample from
channel 0 with one from channel 1. Name channel count and whether a count means
per-channel samples, total complex samples, or time-aligned frames. Inspect the
specific API's contract before translating one count into another.

## Do not confuse host representation with transport packing

`BLADERF_FORMAT_SC16_Q11_PACKED` retains the SC16 Q11 representation at the
synchronous host API while using an intermediate packed representation in the
transport path.
Its CPU work and alignment requirements differ. A file containing ordinary
host SC16 Q11 samples therefore does not become a three-byte-per-complex-sample
file merely because transport packing was enabled. The
[packed-format contract](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/include/libbladeRF.h#L2143-L2158)
states this distinction.
The pinned [asynchronous initialization path](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/bladerf.c#L1118-L1129)
rejects this packed format; do not infer asynchronous support from the format
enum alone.

SoapySDR also distinguishes a requested stream format from the native stream
format and its full-scale value. Use those queries rather than assuming every
signed 16-bit I/Q stream has the same normalization. See its
[native-format API](https://pothosware.github.io/SoapySDR/doxygen/latest/classSoapySDR_1_1Device.html).

## Record requests, readbacks, and observations separately

A useful capture record preserves the requested rate and frequency, the values
read back from the device, stream format and scaling, channel layout, source
versions, completed sample count, and any reported discontinuities. Host
timestamps and device sample timestamps have different meanings and should
not share an unlabeled “time” field.

[SigMF](https://sigmf.org/) already provides an interoperable metadata format
for recorded signals, including datasets, capture segments, annotations, and
extension namespaces. Evaluate it before inventing another general recording
format. A SigMF recording describes an acquisition; a board specification
catalog describes source-qualified knowledge about a product. They should be
linked when useful, rather than treated as interchangeable schemas.

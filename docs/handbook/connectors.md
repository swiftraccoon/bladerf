(connectors)=
# Identify connectors and their electrical roles

**Purpose:** identify RF, host, power, clock and expansion connections before
selecting cables or interpreting a pin name. This atlas is based on visual
inspection of Nuand's **REV 1.4 and REV 1.6 schematics**, sheets 2, 4, 7, 11, 13 and
17. Identify the PCB/assembly revision before applying this atlas. A schematic
designator identifies a circuit symbol; a photograph or the board's silkscreen
is needed to locate it on a particular assembly.

Use the matching [REV 1.4 schematic](https://www.nuand.com/bladeRF-2.0-micro-REV1.4.pdf)
or [REV 1.6 schematic](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf).
Both are 17-page documents, and physical PDF page numbers match the sheet numbers
used below. An xA9 FPGA does not select a PCB revision.

## Connector atlas

Directions in this table are relative to the radio. “Input” identifies a signal
or power role; it is not an electrical rating.

| Board label / function | Schematic designator | Connection and direction | Source sheet |
| --- | --- | --- | ---: |
| RX1 | J1 | SMA RF input, receive channel 1 | [4](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=4) |
| TX1 | J2 | SMA RF output, transmit channel 1 | [4](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=4) |
| RX2 | J3 | SMA RF input, receive channel 2 | [4](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=4) |
| TX2 | J4 | SMA RF output, transmit channel 2 | [4](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=4) |
| USB | U21 | USB 3.0 Type-B; bidirectional host data and VBUS power input | [11](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=11) |
| External DC power | J49 | DC barrel input; RAPC712X as drawn | [17](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=17) |
| CLKOUT | J92 | U.FL clock output from the clock-distribution buffer | [13](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=13) |
| CLKIN | J93 | U.FL external input to the clock-distribution buffer | [13](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=13) |
| REFIN | J95 | U.FL reference input to the ADF4002 PLL | [13](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=13) |
| Main expansion | U74 | 60-contact board-to-board connector; BSH-030-01-F-D-A as drawn | [7](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=7) |
| Mini expansion | J51 | Three contacts: two FPGA signals and ground | [7](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=7) |

The four RF designators do **not** number the receivers first: J2 is TX1, while
J3 is RX2. Identify the printed RX/TX label before using a connector number.
Nuand's [front photograph](https://www.nuand.com/wp-content/uploads/2018/08/bladeRF_micro_front_sma.png)
shows the four labeled SMA ports; its
[top photograph](https://www.nuand.com/wp-content/uploads/2018/08/bladeRF_micro_sq.png)
also shows the clock connectors, USB and power end, and expansion connector.
That top image identifies an assembly REV 1.2, so it is orientation evidence,
not proof of component placement or population on REV 1.4/1.6.

## RF connectors can also carry bias power

The SMA paths connect through RF switching and matching networks to the AD9361.
They are not direct access to arbitrary RFIC pins. RX1/RX2 share an RX local
oscillator, and TX1/TX2 share a TX local oscillator; see
{ref}`hardware-and-software` for the tuning consequence.

[Sheet 2](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=2) shows a switched nominal 5 V bias supply distributed to the RF ports.
The RX pair shares `RX_BIAS_EN`; the TX pair shares `TX_BIAS_EN`. The pinned
[bias-tee implementation](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L2999-L3050)
selects one enable bit by **direction**, rather than an independent enable for
each port. A per-channel API argument therefore does not mean independently
switched bias power on RX1 versus RX2. A cable connected to an RX port can have
a different DC condition after changing the RX bias setting through another
channel.

Nuand's [accessory guidance](https://github.com/Nuand/bladeRF/wiki/bladeRF-Accessories)
uses 50 Ω RF loads and advises attenuation for external TX-to-RX loopback. This
page does not establish an absolute maximum input power at a micro SMA connector.
The product table's **+8 dBm CW output is a typical output characteristic**,
not a receive input limit or a guaranteed setting for every frequency. Likewise,
an AD9361 chip-pin rating is not automatically a board-connector rating. See the
[manufacturer specification table](https://www.nuand.com/bladerf-2-0-micro/).

Sheet 4 also contains optional RF access footprints. Both inspected revisions
mark U1 and U77, associated with the RX C inputs, **DNP**. REV 1.6 additionally
shows DNP U.FL footprints J7, J9, J10 and J11 and their DNP coupling links near
the switched RF paths. These are not four additional fitted antenna ports.
Their presence, modification requirements and electrical behavior must be checked
against the actual assembly before treating them as usable connections.

## USB and external power are separate inputs

The USB connector is **Type-B**, as explicitly named on sheet 11; “micro” in the
product name does not mean a Micro-USB connector. U21 carries USB 2 data, the
SuperSpeed pairs, ground and `VBUS_IN`. It is the host connection, not an
expansion GPIO port.

Nuand's [micro product description](https://www.nuand.com/bladerf-2-0-micro/)
specifies USB bus power and an external **5 V DC** option with automatic
switchover. The
[accessory power specification](https://github.com/Nuand/bladeRF/wiki/bladeRF-Accessories)
lists a **5.5 mm outer / 2.5 mm inner** plug, with the inner contact positive
and outer contact ground. Its 1.5–3 A supply-capacity guidance describes a supply
selection range; it is not a claim that the radio always draws that current.
The schematic identifies J49 as RAPC712X and connects its input to `VDC`.

Sheet 17 places a TPS2115A multiplexer between `VDC`, `VBUS_IN`, and the main
rail. Its note says that an absent or below-4 V DC input selects USB VBUS.
That switching threshold is **not** a recommended 4 V operating supply. The
sheet's `ILIM = 2.5 A` design note is also not a guaranteed current budget for
an expansion card or bias-powered accessory. Board load, source behavior and
the complete power path remain relevant.

The [micro-specific accessory section](https://github.com/Nuand/bladeRF/wiki/bladeRF-Accessories)
describes automatic selection. Classic-board J70 power-jumper instructions on
the same wiki page apply to a different board. They are not a micro procedure.
The [FAQ](https://www.nuand.com/frequently-asked-questions/)
describes the power-source and power-monitor readouts; a readout is an observation,
not a replacement for the input specification.

## Three U.FL clock connectors have different jobs

| Connector | Circuit role | Qualified external-interface description |
| --- | --- | --- |
| J92 / CLKOUT | Clock-buffer output | FAQ describes a 38.4 MHz, 3.3 V CMOS clock output. |
| J93 / CLKIN | Alternative clock-buffer input | FAQ requires a 38.4 MHz, 3.3 V CMOS source for this mode. |
| J95 / REFIN | PLL reference input, AC-coupled by C93 | Reference for disciplining the onboard oscillator; not the direct CLKIN path. |

These roles agree between the visually inspected sheet-13 circuits. The green
“UFL pins are 3.3V” annotation encloses J92/J93; it should not be generalized
into one universal rating for every U.FL footprint on the board. Clock-frequency,
amplitude, waveform and slew conditions are separate constraints. The
[FAQ](https://www.nuand.com/frequently-asked-questions/) calls CLKOUT “U92”; the
schematics designate the connector **J92**. The schematic name is used here.

See {ref}`clocks-and-synchronization` before choosing a reference or a
synchronization arrangement. Sharing a connector family does not make the three
clock ports interchangeable, and clock sharing alone does not establish RF phase
alignment or sample-start alignment.

## Main expansion connector U74

The following is a **schematic contact map**, not a view into the mating face.
Do not infer cable orientation from the odd/even columns. Confirm pin 1,
connector orientation and the mating part against the actual board and connector
drawing. The assignments below agree on sheet 7 of REV 1.4 and REV 1.6.

| Contacts | Schematic nets / role |
| --- | --- |
| 1 | `XB_VCCIO` |
| 2 | `XB_CLK_3P3V` |
| 3 / 4 | `RX_EXT_LO` / `TX_EXT_LO`; named RFIC-related nets, not ordinary GPIO |
| 5 / 7 | `XB_I2C_SCL` / `XB_I2C_SDA` |
| 6 / 8 | `XB_PRESENT` / `XB_CLK_EN` |
| 9 / 10 | `XB_VCCIO_SEL1` / `XB_VCCIO_SEL2` through R142/R143 |
| 57 / 58 | `V5P0` |
| 15, 16, 21, 22, 27, 28, 33, 34, 39, 40, 45, 46, 51, 52, 59, 60 | Ground |

The remaining 32 contacts are the indexed `XB_GPIO` signals:

| Odd contact | GPIO index | Even contact | GPIO index |
| ---: | ---: | ---: | ---: |
| 11 | 31 | 12 | 15 |
| 13 | 30 | 14 | 14 |
| 17 | 29 | 18 | 13 |
| 19 | 28 | 20 | 12 |
| 23 | 27 | 24 | 11 |
| 25 | 26 | 26 | 10 |
| 29 | 25 | 30 | 9 |
| 31 | 24 | 32 | 8 |
| 35 | 23 | 36 | 7 |
| 37 | 22 | 38 | 6 |
| 41 | 21 | 42 | 5 |
| 43 | 20 | 44 | 4 |
| 47 | 19 | 48 | 3 |
| 49 | 18 | 50 | 2 |
| 53 | 17 | 54 | 1 |
| 55 | 16 | 56 | 0 |

The net labels also identify clock-capable and paired functions. A GPIO index
is **not** its physical contact number. The
[pinned FPGA pin assignments](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/hdl/fpga/platforms/bladerf-micro/constraints/pins.tcl#L340-L386)
map `exp_gpio` indices to FPGA package pins; the loaded design determines their
use and output enables.

Sheet 17 provides hardware selection for 1.8, 2.5 or 3.3 V expansion-bank VCCIO,
with 3.3 V identified as the default. The inspected
[HDL constraints](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/hdl/fpga/platforms/bladerf-micro/constraints/pins.tcl#L384-L438)
specify 3.3 V bank supplies and 3.3-V LVCMOS for expansion signals. Hardware
selection options are not permission to change the voltage while retaining
arbitrary FPGA constraints. No 5 V tolerance or general-purpose power-output
current rating is established by this map.

Mechanical similarity also does not establish software support for a classic
expansion card. In libbladeRF 2.6.0, the micro's
[`expansion_attach` implementation](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L2878-L2891)
returns unsupported. This does not mean that the FPGA connector has no usable
signals; it means the classic card-attachment API is not a compatibility promise.

## Mini expansion J51

| Contact | Schematic net | Pinned FPGA assignment |
| ---: | --- | --- |
| 1 | `MINI_EXP_1` | `mini_exp1`, package pin H16 |
| 2 | `MINI_EXP_2` | `mini_exp2`, package pin H15 |
| 3 | Ground | Ground |

J51 is distinct from U74. The three-contact map comes from sheet 7, with FPGA
assignments confirmed in the pinned constraints linked above. A loaded FPGA image
may give these signals a trigger or other application-specific role. Do not
interpret a familiar header shape as a serial-console or arbitrary GPIO pinout.

## Applicability and limits

The atlas identifies schematic connections. It does not establish
an individual board's revision/population, SMA absolute maximum input power,
bias-tee output-current guarantee, expansion power budget, complete timing
constraints, or every connector's insertion/cycle limits. Those require matching
board, component and operational evidence.

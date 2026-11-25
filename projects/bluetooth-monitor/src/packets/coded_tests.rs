use super::*;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        for _ in 0..128 {
            let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bluetooth-coded-packet-test-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated test directory: {error}"),
            }
        }
        panic!("no isolated test directory available")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn packet_record(linktype: u32) -> PacketRecord {
    PacketRecord::new(
        1,
        0,
        0,
        &Header {
            byte_order: ByteOrder::Little,
            nanoseconds: false,
            snaplen: 65_536,
            linktype,
        },
    )
}

fn ble_body(pdu: &[u8], init: u32, access_address: u32) -> Vec<u8> {
    let mut body = access_address.to_le_bytes().to_vec();
    body.extend_from_slice(pdu);
    body.extend_from_slice(&crc24(pdu, init));
    body
}

fn phdr_record(
    pdu: &[u8],
    init: u32,
    access_address: u32,
    phy_bits: u8,
    raw_coding_indicator: Option<u8>,
) -> Vec<u8> {
    let mut packet = vec![0; 10];
    packet[0] = 12;
    let flags = DEWHITENED | (u16::from(phy_bits) << 14);
    packet[8..10].copy_from_slice(&flags.to_le_bytes());
    packet.extend_from_slice(&access_address.to_le_bytes());
    if let Some(indicator) = raw_coding_indicator {
        packet.push(indicator);
    }
    packet.extend_from_slice(pdu);
    packet.extend_from_slice(&crc24(pdu, init));
    packet
}

fn analyze_record(
    packet: &[u8],
    linktype: u32,
    summary: &mut Summary,
    connections: &mut Connections,
    duplicates: &mut DuplicateTracker,
) -> (Option<Vec<u8>>, PacketRecord) {
    let mut record = packet_record(linktype);
    let output = analyze(
        packet,
        linktype,
        &mut record,
        summary,
        connections,
        duplicates,
    );
    (output, record)
}

fn little_endian_pcap(linktype: u32, packets: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0xa1b2_c3d4_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&65_536_u32.to_le_bytes());
    bytes.extend_from_slice(&linktype.to_le_bytes());
    for packet in packets {
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&1_234_u32.to_le_bytes());
        bytes.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        bytes.extend_from_slice(packet);
    }
    bytes
}

#[test]
fn selected_coded_counts_require_both_phy_and_frequency_after_crc() {
    let directory = TestDirectory::new();
    let input = directory.0.join("mixed.pcap");
    let advertising = |physical_channel, phy, ci| {
        let mut packet = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, phy, ci);
        packet[0] = physical_channel;
        packet
    };
    let mut bad = advertising(21, 2, Some(1));
    *bad.last_mut().unwrap() ^= 1;
    fs::write(
        &input,
        little_endian_pcap(
            DLT_BLE_PHDR,
            &[
                advertising(21, 2, Some(0)),
                advertising(21, 2, Some(1)),
                advertising(20, 2, Some(0)),
                advertising(21, 0, None),
                bad,
            ],
        ),
    )
    .unwrap();
    let (summary, selected) = inspect_with_selected_coded(&input, None, None, Some(2444)).unwrap();
    let selected = selected.unwrap();
    assert_eq!(summary.validated_advertising_packets, 4);
    assert_eq!(summary.validated_packets_by_phy.le_coded_s8, 2);
    assert_eq!(summary.invalid_crc_packets, 1);
    assert_eq!(selected.frequency_mhz, 2444);
    assert_eq!((selected.le_coded_s2, selected.le_coded_s8), (1, 1));
}

#[test]
fn validates_coded_advertising_and_data_for_both_coding_schemes() {
    let access_address = 0xaa08_192b;
    let crc_init = 0xc4c181;
    let data_s8 = [0x02, 3, 0x11, 0x22, 0x33];
    let data_s2_with_cte = [0x22, 2, 0x7f, 0x44, 0x55];
    let packets = [
        (
            phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 2, Some(0)),
            0,
            "s8",
        ),
        (
            phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 2, Some(1)),
            1,
            "s2",
        ),
        (
            phdr_record(&data_s8, crc_init, access_address, 2, Some(0)),
            0,
            "s8",
        ),
        (
            phdr_record(&data_s2_with_cte, crc_init, access_address, 2, Some(1)),
            1,
            "s2",
        ),
    ];
    let mut summary = Summary::default();
    let mut connections = Connections::default();
    connections.0.push_back(ConnectionContext {
        access_address,
        crc_init,
        requested_at: (0, 0),
    });
    let mut duplicates = DuplicateTracker::default();

    for (packet, coding_indicator, coding_scheme) in packets {
        let (output, record) = analyze_record(
            &packet,
            DLT_BLE_PHDR,
            &mut summary,
            &mut connections,
            &mut duplicates,
        );
        assert!(output.is_some());
        assert_eq!(record.upstream_phy_bits, Some(2));
        assert_eq!(record.upstream_coding_indicator, Some(coding_indicator));
        assert_eq!(record.coding_scheme, Some(coding_scheme));
    }

    assert_eq!(summary.validated_advertising_packets, 2);
    assert_eq!(summary.validated_data_packets, 2);
    assert_eq!(summary.validated_packets_by_phy.le_coded_s8, 2);
    assert_eq!(summary.validated_packets_by_phy.le_coded_s2, 2);
    validate_duplicate_accounting(&summary).unwrap();
    validate_phy_accounting(&summary).unwrap();
}

#[test]
fn connection_context_follows_capture_timestamps() {
    const ADDRESS: u32 = 0xaa08_192b;
    const CRC_INIT: u32 = 0xc4c181;
    let coded_record = |pdu: &[u8], address, init, ci| {
        let mut bytes = phdr_record(pdu, init, address, 2, Some(ci));
        bytes[0] = 21; // 2444 MHz
        bytes
    };
    let request = |init: u32| {
        let mut pdu = [0; 36];
        pdu[..2].copy_from_slice(&[5, 34]);
        pdu[14..18].copy_from_slice(&ADDRESS.to_le_bytes());
        pdu[18..21].copy_from_slice(&init.to_le_bytes()[..3]);
        pdu[21] = 2; // Window size
        pdu[24] = 24; // Connection interval
        pdu[28] = 100; // Supervision timeout
        pdu[30..35].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x1f]);
        pdu[35] = 5; // Hop increment
        coded_record(&pdu, ADVERTISING_AA, 0x555555, 0)
    };
    let data = |init| coded_record(&[0x16, 5, 1, 2, 3, 4, 5], ADDRESS, init, 1);
    let mut summary = Summary::default();
    let mut connections = Connections::default();
    let mut duplicates = DuplicateTracker::default();
    for (seconds, fraction, packet, expected) in [
        (4, 100, request(CRC_INIT), "advertising_crc_valid"),
        (4, 99, data(CRC_INIT), "crc_unknown_connection_context"),
        (3, 999_999, request(CRC_INIT ^ 1), "advertising_crc_valid"),
        (4, 100, data(CRC_INIT), "connection_crc_valid"),
        (4, 101, data(CRC_INIT ^ 1), "crc_invalid"),
        (4, 102, request(CRC_INIT ^ 1), "advertising_crc_valid"),
        (4, 101, data(CRC_INIT ^ 1), "crc_unknown_connection_context"),
        (4, 103, data(CRC_INIT ^ 1), "connection_crc_valid"),
    ] {
        let mut record = packet_record(DLT_BLE_PHDR);
        record.timestamp_seconds = seconds;
        record.timestamp_fraction = fraction;
        let output = analyze(
            &packet,
            DLT_BLE_PHDR,
            &mut record,
            &mut summary,
            &mut connections,
            &mut duplicates,
        );
        assert_eq!(record.validation, expected);
        assert_eq!(output.is_some(), expected.ends_with("_crc_valid"));
    }
    assert_eq!(summary.validated_advertising_packets, 3);
    assert_eq!(summary.connections_learned, 2);
    assert_eq!(summary.validated_data_packets, 2);
}

#[test]
fn ci_is_outside_crc_and_normalized_duplicate_identity() {
    let mut s8 = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 2, Some(0));
    let mut s2 = s8.clone();
    s2[14] = 1;
    let mut summary = Summary::default();
    let mut connections = Connections::default();
    let mut duplicates = DuplicateTracker::default();

    for packet in [&s8, &s2] {
        let (output, _) = analyze_record(
            packet,
            DLT_BLE_PHDR,
            &mut summary,
            &mut connections,
            &mut duplicates,
        );
        assert!(output.is_some());
    }
    assert_eq!(
        summary.duplicate_accounting.distinct_validated_frame_bodies,
        1
    );
    assert_eq!(summary.duplicate_accounting.repeated_validated_packets, 1);
    assert_eq!(summary.validated_packets_by_phy.le_coded_s8, 1);
    assert_eq!(summary.validated_packets_by_phy.le_coded_s2, 1);

    s8[17] ^= 1;
    let (output, record) = analyze_record(
        &s8,
        DLT_BLE_PHDR,
        &mut summary,
        &mut connections,
        &mut duplicates,
    );
    assert!(output.is_none());
    assert_eq!(record.validation, "crc_invalid");
    assert_eq!(summary.invalid_crc_packets, 1);
    validate_duplicate_accounting(&summary).unwrap();
    validate_phy_accounting(&summary).unwrap();
}

#[test]
fn rejects_reserved_missing_contradictory_and_truncated_coded_layouts() {
    let reserved_phy = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 3, Some(0));
    let reserved_indicators = [2, 3].map(|indicator| {
        phdr_record(
            SPEC_ADVERTISING_PDU,
            0x555555,
            ADVERTISING_AA,
            2,
            Some(indicator),
        )
    });
    let mut missing_indicator = vec![0; 10];
    missing_indicator[0] = 12;
    missing_indicator[8..10].copy_from_slice(&(DEWHITENED | (2 << 14)).to_le_bytes());
    missing_indicator.extend_from_slice(&ADVERTISING_AA.to_le_bytes());
    let mut truncated = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 2, Some(0));
    truncated.pop();
    let contradictory = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 0, Some(0));
    let mut summary = Summary::default();
    let mut connections = Connections::default();
    let mut duplicates = DuplicateTracker::default();

    let (output, record) = analyze_record(
        &reserved_phy,
        DLT_BLE_PHDR,
        &mut summary,
        &mut connections,
        &mut duplicates,
    );
    assert!(output.is_none());
    assert_eq!(record.validation, "unsupported_phy");
    assert!(record.warnings.contains(&"reserved_ble_phy"));

    for (expected, packet) in reserved_indicators.into_iter().enumerate() {
        let (output, record) = analyze_record(
            &packet,
            DLT_BLE_PHDR,
            &mut summary,
            &mut connections,
            &mut duplicates,
        );
        assert!(output.is_none());
        assert_eq!(record.validation, "unsupported_coding_indicator");
        assert_eq!(record.upstream_coding_indicator, Some((expected + 2) as u8));
        assert!(record.warnings.contains(&"reserved_coding_indicator"));
    }

    let (output, record) = analyze_record(
        &missing_indicator,
        DLT_BLE_PHDR,
        &mut summary,
        &mut connections,
        &mut duplicates,
    );
    assert!(output.is_none());
    assert!(record.warnings.contains(&"missing_coded_coding_indicator"));

    for packet in [&truncated, &contradictory] {
        let (output, record) = analyze_record(
            packet,
            DLT_BLE_PHDR,
            &mut summary,
            &mut connections,
            &mut duplicates,
        );
        assert!(output.is_none());
        assert!(
            record
                .warnings
                .contains(&"ble_length_does_not_match_record")
        );
    }

    assert_eq!(summary.unsupported_records, 3);
    assert_eq!(summary.malformed_packets, 3);
    assert_eq!(summary.validated_packets_by_phy.total(), Some(0));
    validate_duplicate_accounting(&summary).unwrap();
    validate_phy_accounting(&summary).unwrap();
}

#[test]
fn accepts_upper_ci_bits_with_a_warning_and_preserves_raw_octet() {
    let packet = phdr_record(
        SPEC_ADVERTISING_PDU,
        0x555555,
        ADVERTISING_AA,
        2,
        Some(0x81),
    );
    let mut summary = Summary::default();
    let mut connections = Connections::default();
    let mut duplicates = DuplicateTracker::default();
    let (output, record) = analyze_record(
        &packet,
        DLT_BLE_PHDR,
        &mut summary,
        &mut connections,
        &mut duplicates,
    );

    assert_eq!(output.as_ref().unwrap()[14], 0x81);
    assert_eq!(record.upstream_coding_indicator, Some(1));
    assert_eq!(record.coding_scheme, Some("s2"));
    assert!(
        record
            .warnings
            .contains(&"coding_indicator_upper_bits_nonzero")
    );
    assert_eq!(summary.validated_packets_by_phy.le_coded_s2, 1);
}

#[test]
fn dewhitens_only_the_coded_pdu_and_crc() {
    let dewhitened = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 2, Some(1));
    let mut whitened = dewhitened.clone();
    let flags = le16(&whitened[8..]) & !DEWHITENED;
    whitened[8..10].copy_from_slice(&flags.to_le_bytes());
    dewhiten(&mut whitened[15..], 38);
    let mut summary = Summary::default();
    let mut connections = Connections::default();
    let mut duplicates = DuplicateTracker::default();
    let (output, record) = analyze_record(
        &whitened,
        DLT_BLE_PHDR,
        &mut summary,
        &mut connections,
        &mut duplicates,
    );
    let output = output.unwrap();

    assert_eq!(output[14], 1);
    assert_eq!(&output[10..], &dewhitened[10..]);
    assert_eq!(record.validation, "advertising_crc_valid");
    assert_eq!(le16(&output[8..]) >> 14, 2);
    assert_ne!(le16(&output[8..]) & DEWHITENED, 0);
}

#[test]
fn uncoded_and_raw_linktypes_remain_in_distinct_phy_buckets() {
    let raw = ble_body(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA);
    let one_megabit = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 0, None);
    let two_megabit = phdr_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA, 1, None);
    let mut summary = Summary::default();
    let mut connections = Connections::default();
    let mut duplicates = DuplicateTracker::default();

    for (linktype, packet, phy_bits) in [
        (DLT_BLE, &raw, None),
        (DLT_BLE_PHDR, &one_megabit, Some(0)),
        (DLT_BLE_PHDR, &two_megabit, Some(1)),
    ] {
        let (output, record) = analyze_record(
            packet,
            linktype,
            &mut summary,
            &mut connections,
            &mut duplicates,
        );
        assert!(output.is_some());
        assert_eq!(record.upstream_phy_bits, phy_bits);
        assert_eq!(record.upstream_coding_indicator, None);
        assert_eq!(record.coding_scheme, None);
    }

    assert_eq!(summary.validated_packets_by_phy.unknown, 1);
    assert_eq!(summary.validated_packets_by_phy.le_1m, 1);
    assert_eq!(summary.validated_packets_by_phy.le_2m, 1);
    assert_eq!(summary.validated_packets_by_phy.le_coded_s8, 0);
    assert_eq!(summary.validated_packets_by_phy.le_coded_s2, 0);
    validate_duplicate_accounting(&summary).unwrap();
    validate_phy_accounting(&summary).unwrap();
}

#[test]
fn validated_pcap_roundtrip_preserves_coded_phy_and_raw_ci() {
    let directory = TestDirectory::new();
    let input = directory.0.join("coded-input.pcap");
    let validated = directory.0.join("coded-validated.pcap");
    let jsonl = directory.0.join("coded.jsonl");
    let packet = phdr_record(
        SPEC_ADVERTISING_PDU,
        0x555555,
        ADVERTISING_AA,
        2,
        Some(0x81),
    );
    fs::write(
        &input,
        little_endian_pcap(DLT_BLE_PHDR, std::slice::from_ref(&packet)),
    )
    .unwrap();

    let summary = inspect(&input, Some(&validated), Some(&jsonl)).unwrap();
    assert_eq!(summary.validated_packets_by_phy.le_coded_s2, 1);
    let bytes = fs::read(&validated).unwrap();
    let saved_packet = &bytes[40..];
    assert_eq!(le16(&saved_packet[8..]) >> 14, 2);
    assert_eq!(saved_packet[14], 0x81);

    let record: Value = serde_json::from_str(fs::read_to_string(&jsonl).unwrap().trim()).unwrap();
    assert_eq!(record["upstream_coding_indicator"], 1);
    assert_eq!(record["coding_scheme"], "s2");
    assert_eq!(record["upstream_phy_bits"], 2);
    assert!(
        record["warnings"]
            .as_array()
            .unwrap()
            .contains(&Value::String(
                "coding_indicator_upper_bits_nonzero".to_owned()
            ))
    );

    let roundtrip = inspect(&validated, None, None).unwrap();
    assert_eq!(roundtrip.validated_advertising_packets, 1);
    assert_eq!(roundtrip.validated_packets_by_phy.le_coded_s2, 1);
}

#[test]
fn rejects_incomplete_validated_phy_partitions() {
    let mut summary = Summary {
        validated_advertising_packets: 1,
        ..Summary::default()
    };
    assert!(validate_phy_accounting(&summary).is_err());
    summary.validated_packets_by_phy.unknown = 1;
    validate_phy_accounting(&summary).unwrap();
}

use super::*;
use serde_json::{Value, json};
use std::os::unix::fs::symlink;

fn write_jsonl(path: &Path, records: &[Value]) {
    let mut file = File::create(path).unwrap();
    for record in records {
        use std::io::Write;
        serde_json::to_writer(&mut file, record).unwrap();
        file.write_all(b"\n").unwrap();
    }
}

fn wifi(index: u64, validation: &str, nanoseconds: u32) -> Value {
    json!({
        "index": index,
        "validation": validation,
        "timestamp_precision": "nanoseconds",
        "timestamp_seconds": 0,
        "timestamp_fraction": nanoseconds,
        "packet_hex": format!("{WIFI_RADIOTAP_HEX}010203040506")
    })
}

fn ble(index: u64, micros: u32, frequency: u16, phy: u8) -> Value {
    json!({
        "index": index,
        "validation": "connection_crc_valid",
        "timestamp_precision": "microseconds",
        "timestamp_seconds": 0,
        "timestamp_fraction": micros,
        "ble_dewhitened_hex": "10111213141516171819",
        "frequency_mhz": frequency,
        "upstream_phy_bits": phy
    })
}

#[test]
fn wifi_packet_positions_rebase_and_invalid_frames_are_excluded() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("packets.jsonl");
    write_jsonl(
        &path,
        &[
            wifi(1, "fcs_absent", 0),
            wifi(2, "fcs_valid_malformed_mac", 50),
            wifi(3, "fcs_valid", 150),
        ],
    );
    let stop = AtomicBool::new(false);
    let packets = read_packets(&path, Protocol::Wifi, 1_000, 4, 1, &stop)
        .unwrap()
        .packets;
    assert_eq!(packets[0].sample, 1_003);
    assert!(read_packets(&path, Protocol::Wifi, 1_000, 3, 1, &stop).is_err());
    assert!(read_packets(&path, Protocol::Wifi, 1_000, 4, 2, &stop).is_err());
}

#[test]
fn ble_rebase_uses_microseconds_and_key_retains_frequency_and_phy() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("packets.jsonl");
    write_jsonl(
        &path,
        &[ble(1, 4, 2426, 0), ble(2, 4, 2428, 0), ble(3, 4, 2426, 1)],
    );
    let stop = AtomicBool::new(false);
    let packets = read_packets(&path, Protocol::Bluetooth, 4_000, 81, 3, &stop)
        .unwrap()
        .packets;
    assert!(packets.iter().all(|packet| packet.sample == 4080));
    assert_ne!(packets[0].key, packets[1].key);
    assert_ne!(packets[0].key, packets[2].key);
}

#[test]
fn malformed_or_incomplete_packet_artifacts_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("packets.jsonl");
    let stop = AtomicBool::new(false);
    for record in [
        wifi(1, "fcs_valid", 51),
        wifi(1, "fcs_valid", 1_000_000_000),
        wifi(2, "fcs_valid", 50),
    ] {
        write_jsonl(&path, &[record]);
        assert!(read_packets(&path, Protocol::Wifi, 0, 100, 1, &stop).is_err());
    }
    let mut bad_body = wifi(1, "fcs_valid", 50);
    bad_body["packet_hex"] = json!(format!("{WIFI_RADIOTAP_HEX}01020304050z"));
    write_jsonl(&path, &[bad_body]);
    assert!(read_packets(&path, Protocol::Wifi, 0, 100, 1, &stop).is_err());

    fs::write(&path, serde_json::to_vec(&wifi(1, "fcs_valid", 0)).unwrap()).unwrap();
    assert!(read_packets(&path, Protocol::Wifi, 0, 100, 1, &stop).is_err());
    fs::write(&path, vec![b' '; MAX_LINE_BYTES + 1]).unwrap();
    assert!(read_packets(&path, Protocol::Wifi, 0, 100, 1, &stop).is_err());
    fs::write(&path, []).unwrap();
    assert!(
        read_packets(&path, Protocol::Wifi, 0, 100, 0, &stop)
            .unwrap()
            .packets
            .is_empty()
    );
    let link = directory.path().join("packet-link.jsonl");
    symlink(&path, &link).unwrap();
    assert!(read_packets(&link, Protocol::Wifi, 0, 100, 0, &stop).is_err());
    assert!(read_packets(&path, Protocol::Wifi, 0, 100, 0, &AtomicBool::new(true)).is_err());
}

fn packet(key_byte: u8, sample: u64) -> Packet {
    Packet {
        key: [key_byte; 32],
        sample,
        record_index: 1,
    }
}

#[test]
fn first_feasible_pairs_maximize_occurrence_count_and_respect_body_keys() {
    // Preferring the exact 1:1 pair of timestamps here would lose a match.
    let full = [packet(1, 1), packet(2, 2), packet(1, 0), packet(3, 2)];
    let slices = [packet(1, 2), packet(1, 1), packet(3, 3), packet(4, 2)];
    let result = match_packets(&full, &slices, Protocol::Wifi).unwrap();
    assert_eq!(result.matched, 3);
    assert_eq!(result.delta_samples_min, Some(1));
    assert_eq!(result.delta_samples_max, Some(1));
}

#[test]
fn matching_uses_signed_deltas_and_closed_protocol_tolerance() {
    let full = [packet(1, 1000), packet(2, 1000), packet(3, 1000)];
    let slices = [packet(1, 600), packet(2, 1400), packet(3, 1401)];
    let wifi = match_packets(&full, &slices, Protocol::Wifi).unwrap();
    assert_eq!(wifi.matched, 0);
    assert_eq!(wifi.delta_samples_min, None);
    let ble = match_packets(&full, &slices, Protocol::Bluetooth).unwrap();
    assert_eq!(ble.matched, 2);
    assert_eq!(ble.delta_samples_min, Some(-400));
    assert_eq!(ble.delta_samples_max, Some(400));
}

//! End-to-end integration test for the BACnet/IP driver.
//!
//! Spawns a mock BACnet device (UDP server) and exercises the full
//! open → learn → sync_cur → close lifecycle through the DriverHandle actor.

#![cfg(test)]

use std::collections::HashMap;
use std::time::Duration;

use tokio::net::UdpSocket;

use super::{frame, object::BacnetObject, value::BacnetValue, BacnetDriver};
use crate::drivers::{
    actor::spawn_driver_actor, async_driver::AnyDriver, DriverPointRef, DriverStatus,
};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Obtain an OS-assigned free port by binding TCP briefly.
fn find_free_port() -> u16 {
    use std::net::TcpListener;
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Encode a minimal I-Am frame (unicast BVLL wrapper).
fn encode_i_am(device_instance: u32, max_apdu: u16, segmentation: u8, vendor_id: u16) -> Vec<u8> {
    let obj_id_val: u32 = (8u32 << 22) | (device_instance & 0x3F_FFFF);
    let obj_id = obj_id_val.to_be_bytes();

    let mut apdu: Vec<u8> = vec![
        0x10, 0x00, // Unconfirmed-Request, I-Am service
        0xC4, obj_id[0], obj_id[1], obj_id[2], obj_id[3], // ObjectId
    ];

    // max-apdu: application tag 2 (Unsigned)
    if max_apdu <= 0xFF {
        apdu.extend_from_slice(&[0x21, max_apdu as u8]);
    } else {
        apdu.extend_from_slice(&[0x22, (max_apdu >> 8) as u8, (max_apdu & 0xFF) as u8]);
    }
    // segmentation: application tag 9 (Enumerated)
    apdu.extend_from_slice(&[0x91, segmentation]);
    // vendor-id: application tag 2 (Unsigned)
    if vendor_id <= 0xFF {
        apdu.extend_from_slice(&[0x21, vendor_id as u8]);
    } else {
        apdu.extend_from_slice(&[0x22, (vendor_id >> 8) as u8, (vendor_id & 0xFF) as u8]);
    }

    let total_len = 4u16 + 2 + apdu.len() as u16;
    let mut out = vec![
        0x81,
        0x0A, // BVLL unicast
        (total_len >> 8) as u8,
        (total_len & 0xFF) as u8,
        0x01,
        0x00, // NPDU version=1, control=0 (local data)
    ];
    out.extend_from_slice(&apdu);
    out
}

/// Extract the APDU invoke_id from a received request packet.
///
/// Layout: BVLL(4) + NPDU(2) = 6 bytes, then APDU starts.
/// For a Confirmed-Request APDU: byte[0]=PDU-type, byte[1]=max-segments, byte[2]=invoke_id.
fn extract_invoke_id(data: &[u8], n: usize) -> u8 {
    if n >= 9 {
        data[8]
    } else {
        0
    }
}

/// Spawn the mock BACnet device on `port`.
///
/// The mock handles:
///   1. Who-Is → I-Am (device instance 1001)
///   2. ReadProperty(Device 1001, ObjectList=76) → [AI-0, AI-1, BI-0]
///   3. ReadProperty(AI-0, ObjectName=77) → "TempSensor"
///   4. ReadProperty(AI-1, ObjectName=77) → "HumiditySensor"
///   5. ReadProperty(BI-0, ObjectName=77) → "Occupancy"
///   6. ReadProperty(AI-0, PresentValue=85) → Real(21.5)
///   7. ReadProperty(AI-1, PresentValue=85) → Real(65.0)
///
/// The mock runs until the returned `JoinHandle` is dropped/aborted.
async fn spawn_mock_bacnet_device(port: u16) -> tokio::task::JoinHandle<()> {
    let sock = UdpSocket::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("mock device bind failed");

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];

        // ── 1. Who-Is → I-Am ─────────────────────────────────────────────
        if let Ok((_, from)) = sock.recv_from(&mut buf).await {
            let reply = encode_i_am(1001, 1476, 3, 8);
            let _ = sock.send_to(&reply, from).await;
        }

        // ── 2. ObjectList request ─────────────────────────────────────────
        if let Ok((n, from)) = sock.recv_from(&mut buf).await {
            let invoke_id = extract_invoke_id(&buf, n);
            let object_list = vec![
                BacnetValue::ObjectId {
                    object_type: 0,
                    instance: 0,
                }, // AI-0
                BacnetValue::ObjectId {
                    object_type: 0,
                    instance: 1,
                }, // AI-1
                BacnetValue::ObjectId {
                    object_type: 3,
                    instance: 0,
                }, // BI-0
            ];
            let ack = frame::encode_read_property_ack_multi(invoke_id, 8, 1001, 76, &object_list);
            let _ = sock.send_to(&ack, from).await;
        }

        // ── 3-5. ObjectName requests for AI-0, AI-1, BI-0 ────────────────
        let names = ["TempSensor", "HumiditySensor", "Occupancy"];
        let obj_types = [0u16, 0u16, 3u16];
        let instances = [0u32, 1u32, 0u32];
        for i in 0..3 {
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = extract_invoke_id(&buf, n);
                let name_val = BacnetValue::CharacterString(names[i].to_string());
                let ack = frame::encode_read_property_ack_multi(
                    invoke_id,
                    obj_types[i],
                    instances[i],
                    77,
                    &[name_val],
                );
                let _ = sock.send_to(&ack, from).await;
            }
        }

        // ── 6-7. PresentValue requests for AI-0 and AI-1 ─────────────────
        // AI-0 → Real(21.5), AI-1 → Real(65.0)
        let pv_values = [21.5f32, 65.0f32];
        for pv in pv_values {
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = extract_invoke_id(&buf, n);
                let ack = frame::encode_read_property_ack(
                    invoke_id,
                    0,  // Analog Input
                    0,  // instance (driver uses object config — mock just echoes)
                    85, // PresentValue
                    &BacnetValue::Real(pv),
                );
                let _ = sock.send_to(&ack, from).await;
            }
        }
    })
}

// ── E2E Test ───────────────────────────────────────────────────────────────

/// Full lifecycle: open → learn → sync_cur → close via the DriverHandle actor.
///
/// Assertions (18 total):
///  1-2.  register() and open_all() succeed
///  3.    open_all() result list has 1 entry
///  4.    that entry's inner Result is Ok
///  5.    driver status after open is Ok
///  6.    learn() returns exactly 3 points
///  7-9.  names include "1001-TempSensor", "1001-HumiditySensor", "1001-Occupancy"
///  10-12. kinds are correct (Number / Number / Bool)
///  13-14. address format is correct ("1001:0:0", "1001:0:1")
///  15-16. sync_cur values are correct (21.5 and 65.0)
///  17.   sync_cur returns 2 results
///  18.   close_all() succeeds
#[tokio::test]
async fn e2e_open_learn_sync_close() {
    // ── 1. Spawn mock device ──────────────────────────────────────────────
    let mock_port = find_free_port();
    let _mock = spawn_mock_bacnet_device(mock_port).await;
    // Give the mock time to bind before the driver sends Who-Is.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // ── 2. Configure the driver ───────────────────────────────────────────
    // port=0  → OS-assigned ephemeral bind port (won't conflict with anything)
    // broadcast_port=mock_port → send Who-Is directly to the mock
    // discovery_timeout=300ms → comfortably within a unit-test budget
    let mut driver = BacnetDriver::new("bac-e2e", "127.0.0.1", 0)
        .with_broadcast_port(mock_port)
        .with_discovery_timeout(Duration::from_millis(300));

    // Pre-register the two AI points so sync_cur can find them.
    // (Normally these would come from config; we add them before register.)
    driver.add_object(
        101,
        BacnetObject {
            device_id: 1001,
            object_type: 0, // Analog Input
            instance: 0,
            scale: 1.0,
            offset: 0.0,
            unit: Some("degF".into()),
        },
    );
    driver.add_object(
        102,
        BacnetObject {
            device_id: 1001,
            object_type: 0, // Analog Input
            instance: 1,
            scale: 1.0,
            offset: 0.0,
            unit: Some("%RH".into()),
        },
    );

    // ── 3. Register with DriverHandle ─────────────────────────────────────
    let handle = spawn_driver_actor(16);
    handle
        .register(AnyDriver::Async(Box::new(driver)))
        .await
        .expect("register should succeed"); // assertion 1

    // ── 4. open_all() ─────────────────────────────────────────────────────
    let open_results = handle
        .open_all()
        .await
        .expect("open_all channel should work"); // assertion 2
    assert_eq!(open_results.len(), 1, "expected 1 driver open result"); // assertion 3

    let (driver_id, open_result) = &open_results[0];
    assert_eq!(driver_id.as_str(), "bac-e2e");
    open_result
        .as_ref()
        .expect("open() should succeed for bac-e2e"); // assertion 4

    // Verify driver status is Ok after open.
    let status = handle
        .get_driver_status("bac-e2e")
        .await
        .expect("get_driver_status channel ok")
        .expect("driver should exist");
    assert_eq!(
        status,
        DriverStatus::Ok,
        "driver status should be Ok after open"
    ); // assertion 5

    // ── 5. learn() ────────────────────────────────────────────────────────
    let grid = handle
        .learn("bac-e2e", None)
        .await
        .expect("learn should succeed");

    assert_eq!(
        grid.len(),
        3,
        "expected 3 learn points (AI-0, AI-1, BI-0), got {grid:?}"
    ); // assertion 6

    // Collect names for membership checks.
    let names: Vec<&str> = grid.iter().map(|p| p.name.as_str()).collect();

    assert!(
        names.contains(&"1001-TempSensor"),
        "missing 1001-TempSensor; got {names:?}"
    ); // assertion 7
    assert!(
        names.contains(&"1001-HumiditySensor"),
        "missing 1001-HumiditySensor; got {names:?}"
    ); // assertion 8
    assert!(
        names.contains(&"1001-Occupancy"),
        "missing 1001-Occupancy; got {names:?}"
    ); // assertion 9

    // Verify kind assignments.
    let temp = grid.iter().find(|p| p.name == "1001-TempSensor").unwrap();
    assert_eq!(temp.kind, "Number", "TempSensor should be Number"); // assertion 10

    let humidity = grid
        .iter()
        .find(|p| p.name == "1001-HumiditySensor")
        .unwrap();
    assert_eq!(humidity.kind, "Number", "HumiditySensor should be Number"); // assertion 11

    let occupancy = grid.iter().find(|p| p.name == "1001-Occupancy").unwrap();
    assert_eq!(occupancy.kind, "Bool", "Occupancy should be Bool"); // assertion 12

    // Verify address format "deviceId:objectType:instance".
    assert_eq!(temp.address, "1001:0:0", "TempSensor address mismatch"); // assertion 13
    assert_eq!(
        humidity.address, "1001:0:1",
        "HumiditySensor address mismatch"
    ); // assertion 14

    // ── 6. sync_cur() ─────────────────────────────────────────────────────
    let mut point_map = HashMap::new();
    point_map.insert(
        "bac-e2e".to_string(),
        vec![
            DriverPointRef {
                point_id: 101,
                address: String::new(),
            },
            DriverPointRef {
                point_id: 102,
                address: String::new(),
            },
        ],
    );

    let sync_results = handle
        .sync_all(point_map)
        .await
        .expect("sync_all channel should work");

    assert_eq!(sync_results.len(), 2, "expected 2 sync results"); // assertion 17 (numbered for doc)

    // Map results by point_id for easier lookup.
    let by_point: HashMap<u32, f64> = sync_results
        .into_iter()
        .map(|(_did, pid, res)| {
            let val = res.expect("sync_cur value should be Ok");
            (pid, val)
        })
        .collect();

    let v101 = *by_point.get(&101).expect("point 101 should have a value");
    assert!(
        (v101 - 21.5f64).abs() < 0.01,
        "AI-0 PresentValue should be 21.5, got {v101}"
    ); // assertion 15

    let v102 = *by_point.get(&102).expect("point 102 should have a value");
    assert!(
        (v102 - 65.0f64).abs() < 0.01,
        "AI-1 PresentValue should be 65.0, got {v102}"
    ); // assertion 16

    // ── 7. close_all() ───────────────────────────────────────────────────
    handle.close_all().await.expect("close_all should succeed"); // assertion 18

    // Verify driver transitions to Down after close.
    let status_after = handle
        .get_driver_status("bac-e2e")
        .await
        .expect("get_driver_status channel ok")
        .expect("driver should still exist");
    assert_eq!(
        status_after,
        DriverStatus::Down,
        "driver status should be Down after close"
    ); // assertion 19 (bonus)
}

// ── Narrower unit tests exercising the mock helpers ────────────────────────

/// Verify encode_i_am produces a frame decodable by the production decoder.
#[test]
fn encode_i_am_is_decodable() {
    let frame_bytes = encode_i_am(1001, 1476, 3, 8);
    let (_, apdu) = frame::decode_packet(&frame_bytes).expect("should decode cleanly");
    match apdu {
        frame::Apdu::IAm {
            device_instance,
            max_apdu,
            segmentation,
            vendor_id,
        } => {
            assert_eq!(device_instance, 1001);
            assert_eq!(max_apdu, 1476);
            assert_eq!(segmentation, 3);
            assert_eq!(vendor_id, 8);
        }
        other => panic!("expected IAm, got {other:?}"),
    }
}

/// Verify extract_invoke_id handles short packets gracefully.
#[test]
fn extract_invoke_id_short_packet_returns_zero() {
    let short = [0x81u8, 0x00, 0x00, 0x08]; // only 4 bytes
    assert_eq!(extract_invoke_id(&short, 4), 0);
}

/// Verify that a ReadProperty request encodes a decodable invoke_id at offset 8.
#[test]
fn read_property_invoke_id_is_at_offset_8() {
    let req = frame::encode_read_property(42, 0, 0, 85, None);
    assert!(req.len() >= 9, "request must be at least 9 bytes");
    assert_eq!(req[8], 42, "invoke_id should be at byte index 8");
    assert_eq!(extract_invoke_id(&req, req.len()), 42);
}

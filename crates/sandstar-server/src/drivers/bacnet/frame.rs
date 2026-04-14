//! BACnet/IP byte-level codec: BVLL → NPDU → APDU.
//!
//! # Wire layout
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │ BVLL header (4 bytes)                        │
//! │   0x81          BACnet/IP magic              │
//! │   type          0x0A unicast / 0x0B broadcast│
//! │   length_hi     total packet length (BE u16) │
//! │   length_lo                                  │
//! ├─────────────────────────────────────────────┤
//! │ NPDU (2+ bytes)                              │
//! │   0x01          version                      │
//! │   control       0x00 = local data            │
//! │   [router fields when control bit 5 is set]  │
//! ├─────────────────────────────────────────────┤
//! │ APDU (variable)                              │
//! └─────────────────────────────────────────────┘
//! ```
//!
//! Only the PDU types and services required for basic ReadProperty
//! polling and WhoIs/IAm discovery are fully decoded.  Unknown PDU types
//! decode to [`Apdu::Other`] rather than returning an error, which keeps
//! the driver robust against unexpected traffic.

use super::value::{decode_application_tag, BacnetValue};
use super::BacnetError;

// ── Constants ──────────────────────────────────────────────

/// BACnet/IP magic byte (always 0x81).
pub const BVLL_BACNET_IP: u8 = 0x81;
/// BVLL type: Original-Unicast-NPDU.
pub const BVLL_UNICAST: u8 = 0x0A;
/// BVLL type: Original-Broadcast-NPDU.
pub const BVLL_BROADCAST: u8 = 0x0B;
/// NPDU version (always 1).
pub const NPDU_VERSION: u8 = 0x01;
/// NPDU control byte: local network, data (no routing).
pub const NPDU_CONTROL_NORMAL: u8 = 0x00;

/// Unconfirmed service: I-Am.
pub const SVC_UNCONFIRMED_IAM: u8 = 0x00;
/// Unconfirmed service: Who-Is.
pub const SVC_UNCONFIRMED_WHOIS: u8 = 0x08;
/// Confirmed service: ReadProperty.
pub const SVC_CONFIRMED_READ_PROPERTY: u8 = 0x0C;

// ── Header structs ─────────────────────────────────────────

/// Decoded BVLL (BACnet Virtual Link Layer) header.
#[derive(Debug, Clone, PartialEq)]
pub struct BvllHeader {
    /// 0x0A = unicast, 0x0B = broadcast.
    pub bvll_type: u8,
    /// Total packet length including these 4 header bytes.
    pub length: u16,
}

/// Decoded NPDU (Network Protocol Data Unit) header.
#[derive(Debug, Clone, PartialEq)]
pub struct NpduHeader {
    /// Always 0x01.
    pub version: u8,
    /// 0x00 = local network, data (no routing information present).
    pub control: u8,
}

// ── APDU enum ──────────────────────────────────────────────

/// Decoded APDU types.
///
/// Fully-decoded variants cover the services needed for discovery and
/// polling.  Everything else falls through to [`Apdu::Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum Apdu {
    /// Unconfirmed Who-Is broadcast discovery request.
    WhoIs {
        low_limit: Option<u32>,
        high_limit: Option<u32>,
    },
    /// Unconfirmed I-Am device advertisement.
    IAm {
        device_instance: u32,
        max_apdu: u16,
        segmentation: u8,
        vendor_id: u16,
    },
    /// Confirmed ReadProperty request.
    ReadPropertyRequest {
        invoke_id: u8,
        object_type: u16,
        instance: u32,
        property_id: u32,
        array_index: Option<u32>,
    },
    /// Complex-ACK ReadProperty response.
    ReadPropertyAck {
        invoke_id: u8,
        object_type: u16,
        instance: u32,
        property_id: u32,
        value: BacnetValue,
    },
    /// Error PDU.
    Error {
        invoke_id: u8,
        service_choice: u8,
        error_class: u32,
        error_code: u32,
    },
    /// Catch-all for PDU types / services not explicitly handled above.
    ///
    /// Used internally by `TransactionTable` tests and returned for
    /// Simple-ACK, Reject, Abort, and unknown Confirmed/Unconfirmed
    /// services.
    Other {
        pdu_type: u8,
        invoke_id: u8,
        data: Vec<u8>,
    },
}

// ── Encoding ───────────────────────────────────────────────

/// Encode a Who-Is broadcast frame.
///
/// If `low` and `high` are both `None`, the frame requests all devices
/// (no range limits).  When a range is supplied both limits must be
/// provided (BACnet spec requires them together).
///
/// # Known-good output (no range)
/// ```text
/// [0x81, 0x0B, 0x00, 0x08, 0x01, 0x00, 0x10, 0x08]
///  BVLL  type  len=8        ver  ctrl  PDU   svc
/// ```
pub fn encode_who_is(low: Option<u32>, high: Option<u32>) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x10, // PDU type = Unconfirmed-Request
        SVC_UNCONFIRMED_WHOIS,
    ];

    if let (Some(lo), Some(hi)) = (low, high) {
        encode_context_unsigned(&mut apdu, 0, lo);
        encode_context_unsigned(&mut apdu, 1, hi);
    }

    build_packet(BVLL_BROADCAST, apdu)
}

/// Encode a ReadProperty-Request (Confirmed-Request) frame.
///
/// # Arguments
/// * `invoke_id`   – 0–255 transaction ID
/// * `object_type` – BACnet object type (0=AI, 1=AO, 2=AV, 8=Device, …)
/// * `instance`    – Object instance number (22 bits max)
/// * `property_id` – Property identifier (e.g. 85 = Present_Value)
/// * `array_index` – Optional array index for array properties
pub fn encode_read_property(
    invoke_id: u8,
    object_type: u16,
    instance: u32,
    property_id: u32,
    array_index: Option<u32>,
) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x00, // PDU type = Confirmed-Request, no segmentation
        0x05, // max-segments=unspecified (0), max-apdu=1476 (5)
        invoke_id,
        SVC_CONFIRMED_READ_PROPERTY,
    ];

    // Context tag 0: ObjectIdentifier (4 bytes)
    // Tag byte: (0 << 4) | 0x08 | 4 = 0x0C
    let obj_id = ((object_type as u32) << 22) | (instance & 0x003F_FFFF);
    apdu.push(0x0C);
    apdu.extend_from_slice(&obj_id.to_be_bytes());

    // Context tag 1: PropertyIdentifier
    encode_context_unsigned(&mut apdu, 1, property_id);

    // Context tag 2: ArrayIndex (optional)
    if let Some(idx) = array_index {
        encode_context_unsigned(&mut apdu, 2, idx);
    }

    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a Complex-ACK ReadProperty response.
///
/// Used in tests and potentially for a BACnet server role.
///
/// # Arguments
/// * `invoke_id`   – must match the outstanding request
/// * `object_type` – BACnet object type (0=AI, etc.)
/// * `instance`    – object instance number
/// * `property_id` – property identifier (e.g. 85 = Present_Value)
/// * `value`       – the application-tagged value to include
pub fn encode_read_property_ack(
    invoke_id: u8,
    object_type: u16,
    instance: u32,
    property_id: u32,
    value: &super::value::BacnetValue,
) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x30, // PDU type = Complex-ACK
        invoke_id,
        SVC_CONFIRMED_READ_PROPERTY,
    ];

    // Context tag 0: object-identifier (4 bytes)
    let obj_id = ((object_type as u32) << 22) | (instance & 0x003F_FFFF);
    apdu.push(0x0C); // (tag 0, context class, LVT=4)
    apdu.extend_from_slice(&obj_id.to_be_bytes());

    // Context tag 1: property-identifier
    encode_context_unsigned(&mut apdu, 1, property_id);

    // Context tag 3: property-value opening
    apdu.push(0x3E);

    // Application-tagged value
    match value {
        super::value::BacnetValue::Real(f) => {
            apdu.push(0x44); // app tag 4 (Real), LVT=4
            apdu.extend_from_slice(&f.to_be_bytes());
        }
        super::value::BacnetValue::Unsigned(n) => {
            let v = *n;
            if v <= 0xFF {
                apdu.push(0x21); // app tag 2 (Unsigned), LVT=1
                apdu.push(v as u8);
            } else if v <= 0xFFFF {
                apdu.push(0x22); // LVT=2
                apdu.extend_from_slice(&(v as u16).to_be_bytes());
            } else {
                apdu.push(0x24); // LVT=4
                apdu.extend_from_slice(&v.to_be_bytes());
            }
        }
        super::value::BacnetValue::Boolean(b) => {
            // BACnet Boolean: LVT encodes the value (0=false, 1=true), no following bytes
            apdu.push(if *b { 0x11 } else { 0x10 });
        }
        _ => {
            // Fallback: encode as Real(0.0)
            apdu.push(0x44);
            apdu.extend_from_slice(&0.0f32.to_be_bytes());
        }
    }

    // Context tag 3: property-value closing
    apdu.push(0x3F);

    build_packet(BVLL_UNICAST, apdu)
}

// ── Decoding ───────────────────────────────────────────────

/// Parse the 4-byte BVLL header.
///
/// Returns `BacnetError::MalformedFrame` if:
/// - `data.len() < 4`
/// - `data[0] != 0x81` (not BACnet/IP)
/// - stated length > `data.len()`
pub fn decode_bvll_header(data: &[u8]) -> Result<BvllHeader, BacnetError> {
    if data.len() < 4 {
        return Err(BacnetError::MalformedFrame(format!(
            "BVLL header too short: {} bytes",
            data.len()
        )));
    }
    if data[0] != BVLL_BACNET_IP {
        return Err(BacnetError::MalformedFrame(format!(
            "not BACnet/IP: expected 0x81, got 0x{:02X}",
            data[0]
        )));
    }
    let length = u16::from_be_bytes([data[2], data[3]]);
    if (length as usize) > data.len() {
        return Err(BacnetError::MalformedFrame(format!(
            "BVLL length {} > buffer {}",
            length,
            data.len()
        )));
    }
    Ok(BvllHeader {
        bvll_type: data[1],
        length,
    })
}

/// Parse the NPDU header from `data`, returning `(header, bytes_consumed)`.
///
/// Skips optional routing fields (DNET / DLEN / DADR / hop-count) when
/// control-byte bit 5 is set, and source fields (SNET / SLEN / SADR)
/// when bit 3 is set.
///
/// Returns `BacnetError::MalformedFrame` if version != 1 or buffer is too
/// short.
pub fn decode_npdu(data: &[u8]) -> Result<(NpduHeader, usize), BacnetError> {
    if data.len() < 2 {
        return Err(BacnetError::MalformedFrame(format!(
            "NPDU too short: {} bytes",
            data.len()
        )));
    }
    let version = data[0];
    if version != NPDU_VERSION {
        return Err(BacnetError::MalformedFrame(format!(
            "NPDU version {version} != 1"
        )));
    }
    let control = data[1];
    let mut consumed = 2usize;

    // Bit 5: destination specifier (DNET + DLEN + DADR + hop-count)
    if control & 0x20 != 0 {
        if data.len() < consumed + 3 {
            return Err(BacnetError::MalformedFrame(
                "NPDU router fields truncated".into(),
            ));
        }
        let dlen = data[consumed + 2] as usize; // DLEN byte
        let router_bytes = 2 + 1 + dlen + 1; // DNET(2) + DLEN(1) + DADR(dlen) + hop(1)
        if data.len() < consumed + router_bytes {
            return Err(BacnetError::MalformedFrame(
                "NPDU router DADR truncated".into(),
            ));
        }
        consumed += router_bytes;
    }

    // Bit 3: source specifier (SNET + SLEN + SADR)
    if control & 0x08 != 0 {
        if data.len() < consumed + 3 {
            return Err(BacnetError::MalformedFrame(
                "NPDU source fields truncated".into(),
            ));
        }
        let slen = data[consumed + 2] as usize;
        let src_bytes = 2 + 1 + slen; // SNET(2) + SLEN(1) + SADR(slen)
        if data.len() < consumed + src_bytes {
            return Err(BacnetError::MalformedFrame(
                "NPDU source SADR truncated".into(),
            ));
        }
        consumed += src_bytes;
    }

    Ok((NpduHeader { version, control }, consumed))
}

/// Parse an APDU byte slice (already stripped of BVLL and NPDU headers).
///
/// Returns [`Apdu::Other`] for PDU types / services we don't handle.
pub fn decode_apdu(data: &[u8]) -> Result<Apdu, BacnetError> {
    if data.is_empty() {
        return Err(BacnetError::MalformedFrame("empty APDU".into()));
    }

    let pdu_type = data[0] & 0xF0;

    match pdu_type {
        0x00 => decode_confirmed_request(data),
        0x10 => decode_unconfirmed_request(data),
        0x30 => decode_complex_ack(data),
        0x50 => decode_error_pdu(data),
        _ => Ok(Apdu::Other {
            pdu_type: data[0],
            invoke_id: if data.len() > 1 { data[1] } else { 0 },
            data: data.to_vec(),
        }),
    }
}

/// Full packet decode: BVLL → NPDU → APDU.
///
/// Returns `(npdu_header, apdu)`.
pub fn decode_packet(data: &[u8]) -> Result<(NpduHeader, Apdu), BacnetError> {
    let bvll = decode_bvll_header(data)?;
    let packet = &data[4..bvll.length as usize];
    let (npdu, npdu_len) = decode_npdu(packet)?;
    let apdu = decode_apdu(&packet[npdu_len..])?;
    Ok((npdu, apdu))
}

// ── Internal APDU decoders ─────────────────────────────────

fn decode_confirmed_request(data: &[u8]) -> Result<Apdu, BacnetError> {
    if data.len() < 4 {
        return Err(BacnetError::MalformedFrame(
            "Confirmed-Request too short".into(),
        ));
    }
    let invoke_id = data[2];
    let service = data[3];

    match service {
        SVC_CONFIRMED_READ_PROPERTY => decode_read_property_request(invoke_id, &data[4..]),
        _ => Ok(Apdu::Other {
            pdu_type: data[0],
            invoke_id,
            data: data.to_vec(),
        }),
    }
}

fn decode_unconfirmed_request(data: &[u8]) -> Result<Apdu, BacnetError> {
    if data.len() < 2 {
        return Err(BacnetError::MalformedFrame(
            "Unconfirmed-Request too short".into(),
        ));
    }
    let service = data[1];

    match service {
        SVC_UNCONFIRMED_WHOIS => decode_who_is(&data[2..]),
        SVC_UNCONFIRMED_IAM => decode_i_am(&data[2..]),
        _ => Ok(Apdu::Other {
            pdu_type: data[0],
            invoke_id: 0,
            data: data.to_vec(),
        }),
    }
}

fn decode_complex_ack(data: &[u8]) -> Result<Apdu, BacnetError> {
    if data.len() < 3 {
        return Err(BacnetError::MalformedFrame("Complex-ACK too short".into()));
    }
    let invoke_id = data[1];
    let service = data[2];

    match service {
        SVC_CONFIRMED_READ_PROPERTY => decode_read_property_ack(invoke_id, &data[3..]),
        _ => Ok(Apdu::Other {
            pdu_type: data[0],
            invoke_id,
            data: data.to_vec(),
        }),
    }
}

fn decode_error_pdu(data: &[u8]) -> Result<Apdu, BacnetError> {
    if data.len() < 3 {
        return Err(BacnetError::MalformedFrame("Error PDU too short".into()));
    }
    let invoke_id = data[1];
    let service_choice = data[2];
    let mut pos = 3;

    let error_class = decode_enumerated_at(data, &mut pos)?;
    let error_code = decode_enumerated_at(data, &mut pos)?;

    Ok(Apdu::Error {
        invoke_id,
        service_choice,
        error_class,
        error_code,
    })
}

// ── Service-level decoders ─────────────────────────────────

fn decode_who_is(payload: &[u8]) -> Result<Apdu, BacnetError> {
    if payload.is_empty() {
        return Ok(Apdu::WhoIs {
            low_limit: None,
            high_limit: None,
        });
    }
    let (low, consumed_low) = read_context_unsigned(payload, 0)?;
    let (high, _) = read_context_unsigned(&payload[consumed_low..], 1)?;
    Ok(Apdu::WhoIs {
        low_limit: Some(low),
        high_limit: Some(high),
    })
}

fn decode_i_am(payload: &[u8]) -> Result<Apdu, BacnetError> {
    // 1. Application ObjectId (tag 12, 4 bytes) — device object identifier
    // 2. Application Unsigned (tag 2)           — max-apdu
    // 3. Application Enumerated (tag 9)         — segmentation
    // 4. Application Unsigned (tag 2)           — vendor-id
    let mut pos = 0;

    let (obj_val, n) = decode_application_tag(&payload[pos..])?;
    pos += n;
    let device_instance = match obj_val {
        BacnetValue::ObjectId { instance, .. } => instance,
        _ => {
            return Err(BacnetError::MalformedFrame(
                "I-Am: expected ObjectId".into(),
            ))
        }
    };

    let (max_apdu_val, n) = decode_application_tag(&payload[pos..])?;
    pos += n;
    let max_apdu = match max_apdu_val {
        BacnetValue::Unsigned(v) => v as u16,
        _ => {
            return Err(BacnetError::MalformedFrame(
                "I-Am: expected Unsigned for max-apdu".into(),
            ))
        }
    };

    let (seg_val, n) = decode_application_tag(&payload[pos..])?;
    pos += n;
    let segmentation = match seg_val {
        BacnetValue::Enumerated(v) => v as u8,
        _ => {
            return Err(BacnetError::MalformedFrame(
                "I-Am: expected Enumerated for segmentation".into(),
            ))
        }
    };

    let (vid_val, _) = decode_application_tag(&payload[pos..])?;
    let vendor_id = match vid_val {
        BacnetValue::Unsigned(v) => v as u16,
        _ => {
            return Err(BacnetError::MalformedFrame(
                "I-Am: expected Unsigned for vendor-id".into(),
            ))
        }
    };

    Ok(Apdu::IAm {
        device_instance,
        max_apdu,
        segmentation,
        vendor_id,
    })
}

fn decode_read_property_request(invoke_id: u8, payload: &[u8]) -> Result<Apdu, BacnetError> {
    // Context tag 0: ObjectIdentifier  — tag byte 0x0C (= (0<<4)|0x08|4)
    // Context tag 1: PropertyIdentifier
    // Context tag 2 (optional): ArrayIndex
    if payload.len() < 5 {
        return Err(BacnetError::MalformedFrame(
            "ReadProperty request: ObjectId truncated".into(),
        ));
    }
    if payload[0] != 0x0C {
        return Err(BacnetError::MalformedFrame(format!(
            "ReadProperty request: expected context tag 0 (0x0C), got 0x{:02X}",
            payload[0]
        )));
    }
    let raw_id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let object_type = ((raw_id >> 22) & 0x3FF) as u16;
    let instance = raw_id & 0x003F_FFFF;
    let mut pos = 5;

    let (prop_id, consumed) = read_context_unsigned(&payload[pos..], 1)?;
    pos += consumed;

    // Optional array index: context tag 2
    let array_index = if pos < payload.len() {
        let tag_byte = payload[pos];
        let tag_num = (tag_byte >> 4) & 0x0F;
        let is_context = (tag_byte & 0x08) != 0;
        if is_context && tag_num == 2 {
            let (idx, consumed) = read_context_unsigned(&payload[pos..], 2)?;
            pos += consumed;
            let _ = pos;
            Some(idx)
        } else {
            None
        }
    } else {
        None
    };

    Ok(Apdu::ReadPropertyRequest {
        invoke_id,
        object_type,
        instance,
        property_id: prop_id,
        array_index,
    })
}

fn decode_read_property_ack(invoke_id: u8, payload: &[u8]) -> Result<Apdu, BacnetError> {
    // Context tag 0: ObjectIdentifier   0x0C
    // Context tag 1: PropertyIdentifier 0x19 (1-byte) or 0x1A (2-byte)
    // Context tag 3 opening: 0x3E
    //   Application-tagged value(s)
    // Context tag 3 closing: 0x3F
    if payload.len() < 5 {
        return Err(BacnetError::MalformedFrame(
            "ReadProperty-ACK: ObjectId truncated".into(),
        ));
    }
    if payload[0] != 0x0C {
        return Err(BacnetError::MalformedFrame(format!(
            "ReadProperty-ACK: expected 0x0C, got 0x{:02X}",
            payload[0]
        )));
    }
    let raw_id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let object_type = ((raw_id >> 22) & 0x3FF) as u16;
    let instance = raw_id & 0x003F_FFFF;
    let mut pos = 5;

    let (property_id, consumed) = read_context_unsigned(&payload[pos..], 1)?;
    pos += consumed;

    // Skip optional array-index context tag 2 (0x29 = 1-byte, 0x2A = 2-byte)
    if pos < payload.len() {
        let tag_byte = payload[pos];
        let tag_num = (tag_byte >> 4) & 0x0F;
        let is_context = (tag_byte & 0x08) != 0;
        if is_context && tag_num == 2 {
            let lvt = (tag_byte & 0x07) as usize;
            pos += 1 + lvt;
        }
    }

    // Opening tag 3: 0x3E = (3<<4)|0x0E
    if pos >= payload.len() || payload[pos] != 0x3E {
        return Err(BacnetError::MalformedFrame(format!(
            "ReadProperty-ACK: expected opening tag 3 (0x3E) at pos {pos}, got 0x{:02X}",
            if pos < payload.len() { payload[pos] } else { 0 }
        )));
    }
    pos += 1;

    // Decode the application-tagged value inside the opening/closing context
    let (value, _n) = decode_application_tag(&payload[pos..])?;

    Ok(Apdu::ReadPropertyAck {
        invoke_id,
        object_type,
        instance,
        property_id,
        value,
    })
}

// ── Encoding helpers ───────────────────────────────────────

/// Prepend BVLL header and NPDU to `apdu` bytes, returning the full packet.
fn build_packet(bvll_type: u8, apdu: Vec<u8>) -> Vec<u8> {
    // BVLL(4) + NPDU(2) + APDU
    let total_len = 4 + 2 + apdu.len();
    let mut pkt = Vec::with_capacity(total_len);
    pkt.push(BVLL_BACNET_IP);
    pkt.push(bvll_type);
    pkt.push((total_len >> 8) as u8);
    pkt.push((total_len & 0xFF) as u8);
    pkt.push(NPDU_VERSION);
    pkt.push(NPDU_CONTROL_NORMAL);
    pkt.extend_from_slice(&apdu);
    pkt
}

/// Encode `val` as a context-tagged unsigned integer with the given `tag_num`.
///
/// Chooses the smallest byte width that fits the value (1, 2, or 4 bytes).
fn encode_context_unsigned(buf: &mut Vec<u8>, tag_num: u8, val: u32) {
    let (len, bytes): (u8, [u8; 4]) = if val <= 0xFF {
        (1, [val as u8, 0, 0, 0])
    } else if val <= 0xFFFF {
        let b = (val as u16).to_be_bytes();
        (2, [b[0], b[1], 0, 0])
    } else {
        (4, val.to_be_bytes())
    };
    // Tag byte: (tag_num << 4) | 0x08 (context class) | len
    buf.push((tag_num << 4) | 0x08 | len);
    buf.extend_from_slice(&bytes[..len as usize]);
}

// ── Decoding helpers ───────────────────────────────────────

/// Read a context-tagged unsigned integer with `expected_tag` from `data`.
///
/// Returns `(value, bytes_consumed)`.
fn read_context_unsigned(data: &[u8], expected_tag: u8) -> Result<(u32, usize), BacnetError> {
    if data.is_empty() {
        return Err(BacnetError::MalformedFrame(format!(
            "expected context tag {expected_tag}, got empty slice"
        )));
    }
    let tag_byte = data[0];
    let tag_num = (tag_byte >> 4) & 0x0F;
    let class = (tag_byte >> 3) & 0x01; // 1 = context
    let lvt = (tag_byte & 0x07) as usize;

    if class != 1 {
        return Err(BacnetError::MalformedFrame(format!(
            "expected context tag {expected_tag}, got application byte 0x{tag_byte:02X}"
        )));
    }
    if tag_num != expected_tag {
        return Err(BacnetError::MalformedFrame(format!(
            "expected context tag {expected_tag}, got tag {tag_num}"
        )));
    }
    if data.len() < 1 + lvt {
        return Err(BacnetError::MalformedFrame(format!(
            "context tag {expected_tag} value truncated"
        )));
    }

    let val = match lvt {
        0 => 0u32,
        1 => data[1] as u32,
        2 => u16::from_be_bytes([data[1], data[2]]) as u32,
        3 => ((data[1] as u32) << 16) | ((data[2] as u32) << 8) | (data[3] as u32),
        4 => u32::from_be_bytes([data[1], data[2], data[3], data[4]]),
        _ => {
            return Err(BacnetError::MalformedFrame(format!(
                "context tag {expected_tag} LVT {lvt} too large"
            )))
        }
    };
    Ok((val, 1 + lvt))
}

/// Decode an Enumerated application-tagged value at `data[*pos]`, advancing `*pos`.
fn decode_enumerated_at(data: &[u8], pos: &mut usize) -> Result<u32, BacnetError> {
    let (val, n) = decode_application_tag(&data[*pos..])?;
    *pos += n;
    match val {
        BacnetValue::Enumerated(v) => Ok(v),
        BacnetValue::Unsigned(v) => Ok(v),
        _ => Err(BacnetError::MalformedFrame(
            "expected Enumerated value".into(),
        )),
    }
}

// ── Unit tests ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::bacnet::value as bacnet_value;

    // ── Encoding ───────────────────────────────────────────────

    /// The canonical Who-Is packet with no range must be exactly 8 bytes.
    #[test]
    fn encode_who_is_no_range_known_good() {
        let pkt = encode_who_is(None, None);
        assert_eq!(
            pkt,
            vec![0x81, 0x0B, 0x00, 0x08, 0x01, 0x00, 0x10, 0x08],
            "Who-Is (no range) known-good mismatch"
        );
    }

    /// Length field must equal actual Vec length.
    #[test]
    fn encode_who_is_length_field_correct_no_range() {
        let pkt = encode_who_is(None, None);
        let stated = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        assert_eq!(stated, pkt.len());
    }

    /// Who-Is with range is longer than without, BVLL type is broadcast.
    #[test]
    fn encode_who_is_with_range_longer_and_broadcast() {
        let pkt = encode_who_is(Some(0), Some(127));
        assert!(pkt.len() > 8);
        let stated = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        assert_eq!(stated, pkt.len());
        assert_eq!(pkt[1], BVLL_BROADCAST);
        assert_eq!(pkt[7], SVC_UNCONFIRMED_WHOIS);
    }

    /// Context tag 0 for low=0 must have correct tag byte and value.
    #[test]
    fn encode_who_is_range_context_tag_low_zero() {
        let pkt = encode_who_is(Some(0), Some(10));
        // After BVLL(4) + NPDU(2) + PDUtype(1) + svc(1) = index 8
        // (0<<4)|0x08|1 = 0x09
        assert_eq!(pkt[8], 0x09, "context tag 0, LVT=1");
        assert_eq!(pkt[9], 0x00, "low=0");
    }

    /// ReadProperty for AI-1, PresentValue: verify BVLL, NPDU, invoke_id, ObjectId.
    #[test]
    fn encode_read_property_ai1_present_value_structure() {
        let pkt = encode_read_property(5, 0, 1, 85, None);
        assert_eq!(pkt[0], 0x81);
        assert_eq!(pkt[1], BVLL_UNICAST);
        let stated = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        assert_eq!(stated, pkt.len());
        assert_eq!(pkt[4], 0x01); // NPDU version
        assert_eq!(pkt[5], 0x00); // NPDU control
        assert_eq!(pkt[6], 0x00); // Confirmed-Request
        assert_eq!(pkt[7], 0x05); // max-segs/max-apdu
        assert_eq!(pkt[8], 5); // invoke_id
        assert_eq!(pkt[9], SVC_CONFIRMED_READ_PROPERTY);
        assert_eq!(pkt[10], 0x0C); // context tag 0, LVT=4
                                   // AI(0) instance 1 → 0x00000001
        assert_eq!(&pkt[11..15], &[0x00, 0x00, 0x00, 0x01]);
    }

    /// ReadProperty for Device 1001, ObjectList (property 76).
    #[test]
    fn encode_read_property_device_1001_object_list() {
        let pkt = encode_read_property(1, 8, 1001, 76, None);
        let stated = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        assert_eq!(stated, pkt.len());
        assert_eq!(pkt[8], 1); // invoke_id
        let raw = u32::from_be_bytes([pkt[11], pkt[12], pkt[13], pkt[14]]);
        assert_eq!(((raw >> 22) & 0x3FF) as u16, 8, "object_type Device");
        assert_eq!(raw & 0x003F_FFFF, 1001, "instance 1001");
    }

    /// ReadProperty with array_index is longer.
    #[test]
    fn encode_read_property_with_array_index_is_longer() {
        let without = encode_read_property(0, 8, 1, 76, None);
        let with_idx = encode_read_property(0, 8, 1, 76, Some(3));
        assert!(with_idx.len() > without.len());
        let stated = u16::from_be_bytes([with_idx[2], with_idx[3]]) as usize;
        assert_eq!(stated, with_idx.len());
    }

    /// Frame length field is always consistent — fuzz over several combinations.
    #[test]
    fn encode_read_property_length_consistent_various_params() {
        for &invoke_id in &[0u8, 1, 127, 255] {
            for &instance in &[0u32, 1, 100, 0x3F_FFFF] {
                let pkt = encode_read_property(invoke_id, 0, instance, 85, None);
                let stated = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
                assert_eq!(stated, pkt.len());
            }
        }
    }

    // ── BVLL header decoding ────────────────────────────────────

    #[test]
    fn decode_bvll_too_short_returns_error() {
        assert!(decode_bvll_header(&[0x81, 0x0B, 0x00]).is_err());
    }

    #[test]
    fn decode_bvll_wrong_magic_returns_error() {
        assert!(decode_bvll_header(&[0x00, 0x0B, 0x00, 0x08]).is_err());
    }

    #[test]
    fn decode_bvll_stated_length_exceeds_buffer_returns_error() {
        assert!(decode_bvll_header(&[0x81, 0x0B, 0x00, 0x64]).is_err());
    }

    #[test]
    fn decode_bvll_broadcast_valid() {
        let pkt = encode_who_is(None, None);
        let hdr = decode_bvll_header(&pkt).unwrap();
        assert_eq!(hdr.bvll_type, BVLL_BROADCAST);
        assert_eq!(hdr.length as usize, pkt.len());
    }

    #[test]
    fn decode_bvll_unicast_valid() {
        let pkt = encode_read_property(0, 0, 1, 85, None);
        let hdr = decode_bvll_header(&pkt).unwrap();
        assert_eq!(hdr.bvll_type, BVLL_UNICAST);
        assert_eq!(hdr.length as usize, pkt.len());
    }

    // ── NPDU decoding ──────────────────────────────────────────

    #[test]
    fn decode_npdu_normal_consumed_2() {
        let (hdr, consumed) = decode_npdu(&[0x01, 0x00]).unwrap();
        assert_eq!(hdr.version, 1);
        assert_eq!(hdr.control, 0x00);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn decode_npdu_wrong_version_returns_error() {
        assert!(decode_npdu(&[0x02, 0x00]).is_err());
    }

    #[test]
    fn decode_npdu_too_short_returns_error() {
        assert!(decode_npdu(&[0x01]).is_err());
    }

    /// Router hop (control bit 5): DNET(2)+DLEN(1)+DADR(1)+hop(1) = 5 extra bytes.
    #[test]
    fn decode_npdu_destination_specifier_skipped() {
        let data = [
            0x01, // version
            0x20, // control: destination specifier set
            0x00, 0x01, // DNET = 1
            0x01, // DLEN = 1
            0xAA, // DADR[0]
            0xFF, // hop count
        ];
        let (hdr, consumed) = decode_npdu(&data).unwrap();
        assert_eq!(hdr.control, 0x20);
        // 2 (fixed) + 2 (DNET) + 1 (DLEN) + 1 (DADR) + 1 (hop) = 7
        assert_eq!(consumed, 7);
    }

    #[test]
    fn decode_npdu_destination_specifier_truncated_returns_error() {
        let data = [0x01, 0x20, 0x00]; // too short for router fields
        assert!(decode_npdu(&data).is_err());
    }

    /// Source specifier (control bit 3): SNET(2)+SLEN(1)+SADR(2) = 5 extra bytes.
    #[test]
    fn decode_npdu_source_specifier_skipped() {
        let data = [
            0x01, // version
            0x08, // control: source specifier set
            0x00, 0x01, // SNET = 1
            0x02, // SLEN = 2
            0xAA, 0xBB, // SADR[0..1]
        ];
        let (hdr, consumed) = decode_npdu(&data).unwrap();
        assert_eq!(hdr.control, 0x08);
        // 2 + 2 (SNET) + 1 (SLEN) + 2 (SADR) = 7
        assert_eq!(consumed, 7);
    }

    // ── Round-trip tests ────────────────────────────────────────

    #[test]
    fn round_trip_who_is_no_range() {
        let pkt = encode_who_is(None, None);
        let (npdu, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(npdu.version, 1);
        assert_eq!(
            apdu,
            Apdu::WhoIs {
                low_limit: None,
                high_limit: None
            }
        );
    }

    #[test]
    fn round_trip_who_is_with_range() {
        let pkt = encode_who_is(Some(10), Some(200));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::WhoIs {
                low_limit: Some(10),
                high_limit: Some(200),
            }
        );
    }

    #[test]
    fn round_trip_who_is_zero_zero_range() {
        let pkt = encode_who_is(Some(0), Some(0));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::WhoIs {
                low_limit: Some(0),
                high_limit: Some(0),
            }
        );
    }

    #[test]
    fn round_trip_who_is_two_byte_range() {
        let pkt = encode_who_is(Some(256), Some(512));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::WhoIs {
                low_limit: Some(256),
                high_limit: Some(512),
            }
        );
    }

    #[test]
    fn round_trip_who_is_large_range() {
        let pkt = encode_who_is(Some(1000), Some(4_194_302));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::WhoIs {
                low_limit: Some(1000),
                high_limit: Some(4_194_302),
            }
        );
    }

    #[test]
    fn round_trip_read_property_request_ai1() {
        let pkt = encode_read_property(7, 0, 42, 85, None);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyRequest {
                invoke_id,
                object_type,
                instance,
                property_id,
                array_index,
            } => {
                assert_eq!(invoke_id, 7);
                assert_eq!(object_type, 0);
                assert_eq!(instance, 42);
                assert_eq!(property_id, 85);
                assert_eq!(array_index, None);
            }
            other => panic!("expected ReadPropertyRequest, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_read_property_with_array_index() {
        let pkt = encode_read_property(3, 8, 100, 76, Some(5));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyRequest { array_index, .. } => {
                assert_eq!(array_index, Some(5));
            }
            other => panic!("expected ReadPropertyRequest, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_read_property_ao0_present_value() {
        let pkt = encode_read_property(0, 1, 0, 85, None);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyRequest {
                invoke_id,
                object_type,
                instance,
                property_id,
                ..
            } => {
                assert_eq!(invoke_id, 0);
                assert_eq!(object_type, 1);
                assert_eq!(instance, 0);
                assert_eq!(property_id, 85);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn round_trip_read_property_max_instance() {
        let max = 0x3F_FFFFu32;
        let pkt = encode_read_property(255, 8, max, 75, None);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyRequest { instance, .. } => assert_eq!(instance, max),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── I-Am decoding ──────────────────────────────────────────

    fn make_i_am_packet(device_instance: u32, max_apdu: u16, seg: u8, vendor: u16) -> Vec<u8> {
        let obj_id: u32 = (8u32 << 22) | device_instance;
        let mut pkt: Vec<u8> = vec![
            0x81,
            0x0B,
            0x00,
            0x00, // BVLL (length filled below)
            0x01,
            0x00, // NPDU
            0x10,
            SVC_UNCONFIRMED_IAM, // Unconfirmed-Req, I-Am
            0xC4,                // ObjectId: app tag 12, LVT=4
        ];
        pkt.extend_from_slice(&obj_id.to_be_bytes());
        // max-apdu
        if max_apdu <= 255 {
            pkt.push(0x21);
            pkt.push(max_apdu as u8);
        } else {
            pkt.push(0x22);
            pkt.extend_from_slice(&max_apdu.to_be_bytes());
        }
        // segmentation (Enumerated)
        pkt.push(0x91);
        pkt.push(seg);
        // vendor-id
        if vendor <= 255 {
            pkt.push(0x21);
            pkt.push(vendor as u8);
        } else {
            pkt.push(0x22);
            pkt.extend_from_slice(&vendor.to_be_bytes());
        }
        let len = pkt.len() as u16;
        pkt[2] = (len >> 8) as u8;
        pkt[3] = (len & 0xFF) as u8;
        pkt
    }

    #[test]
    fn decode_i_am_device_389001() {
        let pkt = make_i_am_packet(389001, 1476, 3, 260);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::IAm {
                device_instance,
                max_apdu,
                segmentation,
                vendor_id,
            } => {
                assert_eq!(device_instance, 389001);
                assert_eq!(max_apdu, 1476);
                assert_eq!(segmentation, 3);
                assert_eq!(vendor_id, 260);
            }
            other => panic!("expected IAm, got {other:?}"),
        }
    }

    #[test]
    fn decode_i_am_small_device_instance() {
        let pkt = make_i_am_packet(1, 480, 0, 8);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::IAm {
                device_instance,
                max_apdu,
                segmentation,
                vendor_id,
            } => {
                assert_eq!(device_instance, 1);
                assert_eq!(max_apdu, 480);
                assert_eq!(segmentation, 0);
                assert_eq!(vendor_id, 8);
            }
            other => panic!("expected IAm, got {other:?}"),
        }
    }

    // ── ReadProperty-ACK decoding ───────────────────────────────

    fn make_read_property_ack(
        invoke_id: u8,
        object_type: u16,
        instance: u32,
        property_id: u32,
        value_bytes: &[u8],
    ) -> Vec<u8> {
        let raw_id: u32 = ((object_type as u32) << 22) | instance;
        let mut apdu_bytes: Vec<u8> = vec![
            0x30, invoke_id, 0x0C, // Complex-ACK, invoke, ReadProperty
            0x0C,
        ];
        apdu_bytes.extend_from_slice(&raw_id.to_be_bytes());
        // PropertyId as context tag 1
        if property_id <= 255 {
            apdu_bytes.push(0x19);
            apdu_bytes.push(property_id as u8);
        } else {
            apdu_bytes.push(0x1A);
            apdu_bytes.extend_from_slice(&(property_id as u16).to_be_bytes());
        }
        apdu_bytes.push(0x3E); // opening tag 3
        apdu_bytes.extend_from_slice(value_bytes);
        apdu_bytes.push(0x3F); // closing tag 3
        apdu_bytes
    }

    #[test]
    fn decode_read_property_ack_real_72_5() {
        let real_bytes = 72.5f32.to_be_bytes();
        let mut val = vec![0x44]; // Real tag
        val.extend_from_slice(&real_bytes);
        let apdu_bytes = make_read_property_ack(5, 0, 1, 85, &val);
        let apdu = decode_apdu(&apdu_bytes).unwrap();
        match apdu {
            Apdu::ReadPropertyAck {
                invoke_id,
                object_type,
                instance,
                property_id,
                value,
            } => {
                assert_eq!(invoke_id, 5);
                assert_eq!(object_type, 0);
                assert_eq!(instance, 1);
                assert_eq!(property_id, 85);
                assert_eq!(value, BacnetValue::Real(72.5));
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    #[test]
    fn decode_read_property_ack_boolean_true() {
        let val = [0x11]; // Boolean true
        let apdu_bytes = make_read_property_ack(2, 3, 5, 85, &val);
        let apdu = decode_apdu(&apdu_bytes).unwrap();
        match apdu {
            Apdu::ReadPropertyAck {
                value,
                object_type,
                instance,
                ..
            } => {
                assert_eq!(value, BacnetValue::Boolean(true));
                assert_eq!(object_type, 3); // BI
                assert_eq!(instance, 5);
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    #[test]
    fn decode_read_property_ack_unsigned() {
        let val = [0x21, 0x42]; // Unsigned 66
        let apdu_bytes = make_read_property_ack(1, 0, 2, 85, &val);
        let apdu = decode_apdu(&apdu_bytes).unwrap();
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => {
                assert_eq!(value, BacnetValue::Unsigned(0x42));
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    // ── Error PDU decoding ─────────────────────────────────────

    #[test]
    fn decode_error_pdu_class2_code31() {
        let pkt = [
            0x50, 3, 0x0C, // Error, invoke=3, svc=ReadProperty
            0x91, 0x02, // error-class = Enumerated(2)
            0x91, 0x1F, // error-code = Enumerated(31)
        ];
        let apdu = decode_apdu(&pkt).unwrap();
        match apdu {
            Apdu::Error {
                invoke_id,
                service_choice,
                error_class,
                error_code,
            } => {
                assert_eq!(invoke_id, 3);
                assert_eq!(service_choice, 0x0C);
                assert_eq!(error_class, 2);
                assert_eq!(error_code, 31);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn decode_error_pdu_too_short_returns_error() {
        assert!(decode_apdu(&[0x50, 1]).is_err());
    }

    // ── Malformed / edge cases ─────────────────────────────────

    #[test]
    fn decode_packet_too_short_returns_error() {
        assert!(decode_packet(&[0x81, 0x0B, 0x00]).is_err());
    }

    #[test]
    fn decode_packet_wrong_magic_returns_error() {
        let mut pkt = encode_who_is(None, None);
        pkt[0] = 0x00;
        assert!(decode_packet(&pkt).is_err());
    }

    #[test]
    fn decode_apdu_empty_returns_error() {
        assert!(decode_apdu(&[]).is_err());
    }

    #[test]
    fn decode_apdu_simple_ack_returns_other() {
        // Simple-ACK (0x20) is returned as Other
        let pkt = [0x20, 0x05, 0x0C];
        let apdu = decode_apdu(&pkt).unwrap();
        assert!(matches!(apdu, Apdu::Other { .. }));
    }

    #[test]
    fn decode_apdu_unconfirmed_unknown_service_returns_other() {
        let pkt = [0x10, 0xFF]; // Unconfirmed-Req, unknown service
        let apdu = decode_apdu(&pkt).unwrap();
        assert!(matches!(apdu, Apdu::Other { .. }));
    }

    #[test]
    fn decode_apdu_confirmed_unknown_service_returns_other() {
        let pkt = [0x00, 0x05, 3, 0xFF]; // Confirmed-Req, unknown service
        let apdu = decode_apdu(&pkt).unwrap();
        assert!(matches!(apdu, Apdu::Other { .. }));
    }

    #[test]
    fn round_trip_read_property_av_units() {
        let pkt = encode_read_property(10, 2, 50, 103, None);
        let stated = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        assert_eq!(stated, pkt.len());
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyRequest {
                object_type,
                instance,
                property_id,
                ..
            } => {
                assert_eq!(object_type, 2);
                assert_eq!(instance, 50);
                assert_eq!(property_id, 103);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── Phase B2 edge-case tests ────────────────────────────────

    /// Hand-crafted I-Am packet for device 12345, max_apdu=1476, seg=3, vendor=8.
    #[test]
    fn decode_i_am_known_packet() {
        let device_instance = 12345u32;
        let obj_id_val: u32 = (8u32 << 22) | device_instance;
        let obj_id = obj_id_val.to_be_bytes();

        let mut apdu = vec![
            0x10,
            SVC_UNCONFIRMED_IAM, // Unconfirmed-Request, I-Am
            0xC4,
            obj_id[0],
            obj_id[1],
            obj_id[2],
            obj_id[3], // ObjectId app-tag 12, LVT=4
            0x22,
            0x05,
            0xC4, // max_apdu=1476 (Unsigned 2 bytes: 0x05C4)
            0x91,
            0x03, // segmentation=3 (Enumerated 1 byte)
            0x21,
            0x08, // vendor_id=8 (Unsigned 1 byte)
        ];

        // Wrap in BVLL + NPDU
        let total = (4 + 2 + apdu.len()) as u16;
        let mut packet = vec![
            0x81,
            0x0B,
            (total >> 8) as u8,
            (total & 0xFF) as u8,
            0x01,
            0x00,
        ];
        packet.append(&mut apdu);

        let (_, decoded) = decode_packet(&packet).expect("should decode");
        match decoded {
            Apdu::IAm {
                device_instance: di,
                max_apdu,
                segmentation,
                vendor_id,
            } => {
                assert_eq!(di, 12345);
                assert_eq!(max_apdu, 1476);
                assert_eq!(segmentation, 3);
                assert_eq!(vendor_id, 8);
            }
            other => panic!("expected IAm, got {:?}", other),
        }
    }

    /// Who-Is with full range [0, 4194302] round-trips cleanly.
    #[test]
    fn who_is_with_full_range_round_trip() {
        let encoded = encode_who_is(Some(0), Some(4_194_302));
        let (_, apdu) = decode_packet(&encoded).expect("should decode");
        match apdu {
            Apdu::WhoIs {
                low_limit,
                high_limit,
            } => {
                assert_eq!(low_limit, Some(0));
                assert_eq!(high_limit, Some(4_194_302));
            }
            other => panic!("expected WhoIs, got {:?}", other),
        }
    }

    /// Verify that the 22-bit device instance and object type can be recovered
    /// from the BACnet ObjectId encoding for max instance value 4,194,302.
    #[test]
    fn device_object_id_max_instance() {
        let instance = 4_194_302u32;
        let obj_id_val: u32 = (8u32 << 22) | instance;
        let recovered_instance = obj_id_val & 0x3F_FFFF;
        let recovered_type = (obj_id_val >> 22) as u16;
        assert_eq!(
            recovered_instance, instance,
            "instance bits should round-trip"
        );
        assert_eq!(recovered_type, 8, "object type should be Device (8)");

        // Also verify via the application-tag decoder
        let bytes = obj_id_val.to_be_bytes();
        let data = [0xC4u8, bytes[0], bytes[1], bytes[2], bytes[3]];
        let (val, consumed) = bacnet_value::decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 5);
        match val {
            bacnet_value::BacnetValue::ObjectId {
                object_type,
                instance: decoded_instance,
            } => {
                assert_eq!(object_type, 8);
                assert_eq!(decoded_instance, instance);
            }
            other => panic!("expected ObjectId, got {:?}", other),
        }
    }

    /// Detailed structure check for encode_read_property output bytes.
    #[test]
    fn encode_read_property_structure() {
        // AnalogInput (type 0), instance 1, PresentValue (85 = 0x55), invoke_id=5, no array index
        let frame = encode_read_property(0x05, 0, 1, 85, None);

        // BVLL header
        assert_eq!(frame[0], 0x81, "BVLL magic");
        assert_eq!(frame[1], BVLL_UNICAST, "BVLL unicast type");
        let len = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        assert_eq!(len, frame.len(), "BVLL length field matches actual length");

        // NPDU
        assert_eq!(frame[4], 0x01, "NPDU version");
        assert_eq!(frame[5], 0x00, "NPDU control");

        // APDU: Confirmed-Request
        assert_eq!(frame[6], 0x00, "Confirmed-Request PDU type");
        assert_eq!(frame[7], 0x05, "max-segs/max-apdu byte");
        assert_eq!(frame[8], 0x05, "invoke_id = 5");
        assert_eq!(
            frame[9], SVC_CONFIRMED_READ_PROPERTY,
            "service = ReadProperty"
        );

        // Context tag 0 (ObjectIdentifier): 0x0C = (0 << 4) | 0x08 | 4
        assert_eq!(frame[10], 0x0C, "context tag 0, LVT=4");

        // Object ID: AnalogInput(0) instance 1 → (0 << 22) | 1 = 0x00000001
        let obj_id = u32::from_be_bytes([frame[11], frame[12], frame[13], frame[14]]);
        assert_eq!(obj_id & 0x3F_FFFF, 1, "instance = 1");
        assert_eq!((obj_id >> 22) as u16, 0, "type = AnalogInput(0)");

        // Context tag 1 (PropertyId = 85 = 0x55):
        // 0x19 = (1 << 4) | 0x08 | 1 = tag 1, context, LVT=1
        assert_eq!(frame[15], 0x19, "context tag 1, LVT=1");
        assert_eq!(frame[16], 0x55, "property_id = 85 = PresentValue");
    }

    /// I-Am for device instance 0 (boundary: minimum valid instance).
    #[test]
    fn decode_i_am_instance_zero() {
        let pkt = make_i_am_packet(0, 480, 3, 1);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::IAm {
                device_instance, ..
            } => assert_eq!(device_instance, 0),
            other => panic!("expected IAm, got {other:?}"),
        }
    }

    /// I-Am for max valid BACnet device instance (4,194,302).
    #[test]
    fn decode_i_am_max_instance() {
        let max = 4_194_302u32;
        let pkt = make_i_am_packet(max, 1476, 0, 260);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::IAm {
                device_instance, ..
            } => assert_eq!(device_instance, max),
            other => panic!("expected IAm, got {other:?}"),
        }
    }

    // ── Phase B3: encode_read_property_ack + error/round-trip tests ────────────

    /// Encode a Real value ACK and decode it back — all fields must match.
    #[test]
    fn read_property_ack_real_round_trip() {
        use bacnet_value::BacnetValue;
        let pkt = encode_read_property_ack(42, 0, 5, 85, &BacnetValue::Real(22.5));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::ReadPropertyAck {
                invoke_id: 42,
                object_type: 0,
                instance: 5,
                property_id: 85,
                value: BacnetValue::Real(22.5),
            }
        );
    }

    /// Encode an Unsigned value ACK and decode it back.
    #[test]
    fn read_property_ack_unsigned_round_trip() {
        use bacnet_value::BacnetValue;
        let pkt = encode_read_property_ack(7, 0, 1, 85, &BacnetValue::Unsigned(1234));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyAck {
                invoke_id, value, ..
            } => {
                assert_eq!(invoke_id, 7);
                assert_eq!(value, BacnetValue::Unsigned(1234));
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    /// Encode Boolean(true) ACK and decode it back.
    #[test]
    fn read_property_ack_boolean_true_round_trip() {
        use bacnet_value::BacnetValue;
        let pkt = encode_read_property_ack(1, 3, 0, 85, &BacnetValue::Boolean(true));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyAck {
                invoke_id, value, ..
            } => {
                assert_eq!(invoke_id, 1);
                assert_eq!(value, BacnetValue::Boolean(true));
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    /// Encode Boolean(false) ACK and decode it back.
    #[test]
    fn read_property_ack_boolean_false_round_trip() {
        use bacnet_value::BacnetValue;
        let pkt = encode_read_property_ack(1, 3, 0, 85, &BacnetValue::Boolean(false));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => {
                assert_eq!(value, BacnetValue::Boolean(false));
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    /// Hand-crafted Error PDU — invoke 55, svc ReadProperty, class=2, code=31.
    #[test]
    fn error_pdu_decode() {
        let pkt = [
            0x50, // PDU type = Error
            55,   // invoke_id
            0x0C, // service-choice = ReadProperty
            0x91, 0x02, // error_class = Enumerated(2)
            0x91, 0x1F, // error_code  = Enumerated(31)
        ];
        let apdu = decode_apdu(&pkt).unwrap();
        match apdu {
            Apdu::Error {
                invoke_id,
                service_choice,
                error_class,
                error_code,
            } => {
                assert_eq!(invoke_id, 55);
                assert_eq!(service_choice, 0x0C);
                assert_eq!(error_class, 2);
                assert_eq!(error_code, 31);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// A 2-byte Error PDU must not panic — it returns an error or Apdu::Other.
    #[test]
    fn error_pdu_wraps_to_other_if_truncated() {
        // 2 bytes is below the 3-byte minimum; decode_error_pdu returns Err.
        // decode_apdu propagates that error, so we just assert it doesn't panic.
        let result = decode_apdu(&[0x50, 42]);
        // Either an error or Other is acceptable — the key requirement is no panic.
        match result {
            Err(_) => {}
            Ok(Apdu::Other { .. }) => {}
            Ok(other) => panic!("unexpected successful variant: {other:?}"),
        }
    }

    /// ReadProperty request round-trip: AV type, instance 3, property 85.
    #[test]
    fn read_property_request_encode_decode() {
        let pkt = encode_read_property(10, 2, 3, 85, None);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::ReadPropertyRequest {
                invoke_id: 10,
                object_type: 2,
                instance: 3,
                property_id: 85,
                array_index: None,
            }
        );
    }

    /// ReadProperty request with array_index=Some(0) round-trips correctly.
    #[test]
    fn read_property_request_with_array_index() {
        let pkt = encode_read_property(10, 2, 3, 85, Some(0));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyRequest { array_index, .. } => {
                assert_eq!(array_index, Some(0));
            }
            other => panic!("expected ReadPropertyRequest, got {other:?}"),
        }
    }

    /// Who-Is with no range decodes to WhoIs { None, None }.
    #[test]
    fn who_is_no_range_decode() {
        let pkt = encode_who_is(None, None);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::WhoIs {
                low_limit: None,
                high_limit: None
            }
        );
    }

    /// Who-Is with range [100, 200] decodes correctly.
    #[test]
    fn who_is_with_range_decode() {
        let pkt = encode_who_is(Some(100), Some(200));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::WhoIs {
                low_limit: Some(100),
                high_limit: Some(200)
            }
        );
    }

    /// invoke_id is preserved for boundary values 0, 127, 255.
    #[test]
    fn complex_ack_different_invoke_ids() {
        use bacnet_value::BacnetValue;
        for invoke_id in [0u8, 127, 255] {
            let pkt = encode_read_property_ack(invoke_id, 0, 1, 85, &BacnetValue::Real(1.0));
            let (_, apdu) = decode_packet(&pkt).unwrap();
            match apdu {
                Apdu::ReadPropertyAck {
                    invoke_id: decoded_id,
                    ..
                } => {
                    assert_eq!(
                        decoded_id, invoke_id,
                        "invoke_id {invoke_id} must round-trip"
                    );
                }
                other => {
                    panic!("expected ReadPropertyAck for invoke_id {invoke_id}, got {other:?}")
                }
            }
        }
    }

    /// Encode with instance = 4_194_302 (max 22-bit BACnet instance).
    #[test]
    fn read_property_ack_large_instance() {
        use bacnet_value::BacnetValue;
        let instance = 4_194_302u32; // 0x3F_FFFE
        let pkt = encode_read_property_ack(0, 8, instance, 85, &BacnetValue::Real(0.0));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyAck {
                object_type,
                instance: decoded_inst,
                ..
            } => {
                assert_eq!(object_type, 8, "object_type Device (8)");
                assert_eq!(decoded_inst, instance, "large instance must round-trip");
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }
}

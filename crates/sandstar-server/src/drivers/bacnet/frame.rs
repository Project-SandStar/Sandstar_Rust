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
/// Confirmed service: ReadPropertyMultiple.
pub const SVC_CONFIRMED_READ_PROPERTY_MULTIPLE: u8 = 0x0E;
/// Confirmed service: WriteProperty.
pub const SVC_CONFIRMED_WRITE_PROPERTY: u8 = 0x0F;
/// Confirmed service: SubscribeCOV (Phase B8).
pub const SVC_CONFIRMED_SUBSCRIBE_COV: u8 = 0x05;

// ── ReadPropertyMultiple (Phase B9) ────────────────────────

/// A single property specification within a ReadPropertyMultiple request.
#[derive(Debug, Clone, PartialEq)]
pub struct RpmRequestSpec {
    /// BACnet object type (0=AI, 1=AO, 2=AV, 8=Device, …).
    pub object_type: u16,
    /// BACnet object instance number (22-bit).
    pub instance: u32,
    /// Property identifier (e.g. 85 = Present_Value).
    pub property_id: u32,
    /// Optional array index for array properties.
    pub array_index: Option<u32>,
}

/// A single result within a ReadPropertyMultiple-ACK response.
///
/// `value` is `Ok(BacnetValue)` on success or `Err((class, code))` when the
/// device reports an error for this specific property.
#[derive(Debug, Clone, PartialEq)]
pub struct RpmResult {
    pub object_type: u16,
    pub instance: u32,
    pub property_id: u32,
    pub array_index: Option<u32>,
    pub value: Result<super::value::BacnetValue, (u32, u32)>,
}

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
    /// Confirmed WriteProperty request.
    WritePropertyRequest {
        invoke_id: u8,
        object_type: u16,
        instance: u32,
        property_id: u32,
        array_index: Option<u32>,
        value: BacnetValue,
        priority: Option<u8>,
    },
    /// Simple-ACK PDU (PDU type 0x20) — acknowledges a confirmed service
    /// that returns no result data (e.g. WriteProperty).
    SimpleAck { invoke_id: u8, service_choice: u8 },
    /// Confirmed ReadPropertyMultiple request (Phase B9).
    ReadPropertyMultipleRequest {
        invoke_id: u8,
        specs: Vec<RpmRequestSpec>,
    },
    /// Complex-ACK ReadPropertyMultiple response (Phase B9).
    ReadPropertyMultipleAck {
        invoke_id: u8,
        results: Vec<RpmResult>,
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

/// Encode a Complex-ACK ReadProperty response with MULTIPLE values.
///
/// Used for array properties like ObjectList where the response contains
/// several application-tagged values inside the `3E...3F` wrapper.
pub fn encode_read_property_ack_multi(
    invoke_id: u8,
    object_type: u16,
    instance: u32,
    property_id: u32,
    values: &[super::value::BacnetValue],
) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x30, // Complex-ACK PDU type
        invoke_id, 0x0C, // service-choice = ReadProperty
    ];

    // Context tag 0: ObjectIdentifier
    let obj_id = ((object_type as u32) << 22) | (instance & 0x003F_FFFF);
    apdu.push(0x0C);
    apdu.extend_from_slice(&obj_id.to_be_bytes());

    // Context tag 1: PropertyIdentifier
    encode_context_unsigned(&mut apdu, 1, property_id);

    // Context tag 3 opening
    apdu.push(0x3E);

    // Encode each value
    for val in values {
        encode_bacnet_value(&mut apdu, val);
    }

    // Context tag 3 closing
    apdu.push(0x3F);

    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a ReadPropertyMultiple-Request (Confirmed-Request) frame.
///
/// Each spec is encoded as:
/// - Context tag 0: ObjectIdentifier (4 bytes)
/// - Context tag 1 opening (0x1E)
///     - Context tag 0 (property-id) per property
///     - Optional context tag 1 (array-index)
/// - Context tag 1 closing (0x1F)
///
/// # Arguments
/// * `invoke_id` – 0–255 transaction ID
/// * `specs`     – list of properties to read (from potentially multiple objects)
pub fn encode_read_property_multiple(invoke_id: u8, specs: &[RpmRequestSpec]) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x00, // PDU type = Confirmed-Request, no segmentation
        0x05, // max-segments=unspecified, max-apdu=1476
        invoke_id,
        SVC_CONFIRMED_READ_PROPERTY_MULTIPLE,
    ];

    // Group specs by (object_type, instance) so each BACnet ReadAccessSpecification
    // groups multiple properties for the same object.
    let mut groups: Vec<(u16, u32, Vec<&RpmRequestSpec>)> = Vec::new();
    for spec in specs {
        if let Some(g) = groups
            .iter_mut()
            .find(|(ot, inst, _)| *ot == spec.object_type && *inst == spec.instance)
        {
            g.2.push(spec);
        } else {
            groups.push((spec.object_type, spec.instance, vec![spec]));
        }
    }

    for (object_type, instance, props) in &groups {
        // Context tag 0: ObjectIdentifier (4 bytes)
        let obj_id = ((*object_type as u32) << 22) | (*instance & 0x003F_FFFF);
        apdu.push(0x0C);
        apdu.extend_from_slice(&obj_id.to_be_bytes());

        // Context tag 1 opening: list-of-property-references
        apdu.push(0x1E);

        for spec in props {
            // Inner context tag 0: property-id
            encode_context_unsigned(&mut apdu, 0, spec.property_id);
            // Inner context tag 1: property-array-index (optional)
            if let Some(idx) = spec.array_index {
                encode_context_unsigned(&mut apdu, 1, idx);
            }
        }

        // Context tag 1 closing
        apdu.push(0x1F);
    }

    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a ReadPropertyMultiple-ACK (Complex-ACK) frame.
///
/// Used by the mock server in tests. Each result is encoded as:
/// - Context tag 0: ObjectIdentifier
/// - Context tag 1 opening
///     - Context tag 2: property-id (per result)
///     - Optional context tag 3: property-array-index
///     - Context tag 4 opening (property-value) + application value + closing
///       (on success), OR context tag 5 opening (property-access-error) +
///       error-class + error-code + closing (on failure)
/// - Context tag 1 closing
pub fn encode_read_property_multiple_ack(invoke_id: u8, results: &[RpmResult]) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x30, // PDU type = Complex-ACK
        invoke_id,
        SVC_CONFIRMED_READ_PROPERTY_MULTIPLE,
    ];

    // Group results by (object_type, instance).
    let mut groups: Vec<(u16, u32, Vec<&RpmResult>)> = Vec::new();
    for r in results {
        if let Some(g) = groups
            .iter_mut()
            .find(|(ot, inst, _)| *ot == r.object_type && *inst == r.instance)
        {
            g.2.push(r);
        } else {
            groups.push((r.object_type, r.instance, vec![r]));
        }
    }

    for (object_type, instance, items) in &groups {
        // Context tag 0: ObjectIdentifier
        let obj_id = ((*object_type as u32) << 22) | (*instance & 0x003F_FFFF);
        apdu.push(0x0C);
        apdu.extend_from_slice(&obj_id.to_be_bytes());

        // Context tag 1 opening: list-of-results
        apdu.push(0x1E);

        for r in items {
            // Context tag 2: property-identifier
            encode_context_unsigned(&mut apdu, 2, r.property_id);
            // Context tag 3: property-array-index (optional)
            if let Some(idx) = r.array_index {
                encode_context_unsigned(&mut apdu, 3, idx);
            }

            match &r.value {
                Ok(v) => {
                    // Context tag 4 opening: property-value
                    apdu.push(0x4E);
                    encode_bacnet_value(&mut apdu, v);
                    apdu.push(0x4F); // closing
                }
                Err((class, code)) => {
                    // Context tag 5 opening: property-access-error
                    apdu.push(0x5E);
                    // error-class (Enumerated)
                    if *class <= 0xFF {
                        apdu.extend_from_slice(&[0x91, *class as u8]);
                    } else {
                        apdu.push(0x92);
                        apdu.extend_from_slice(&(*class as u16).to_be_bytes());
                    }
                    // error-code (Enumerated)
                    if *code <= 0xFF {
                        apdu.extend_from_slice(&[0x91, *code as u8]);
                    } else {
                        apdu.push(0x92);
                        apdu.extend_from_slice(&(*code as u16).to_be_bytes());
                    }
                    apdu.push(0x5F); // closing
                }
            }
        }

        // Context tag 1 closing
        apdu.push(0x1F);
    }

    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a BACnet Error PDU (PDU type 0x50).
///
/// Used by mock tests to simulate device-level errors.
pub fn encode_error_pdu(
    invoke_id: u8,
    service_choice: u8,
    error_class: u32,
    error_code: u32,
) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x50, // PDU type = Error
        invoke_id,
        service_choice,
    ];
    // error-class (Enumerated)
    if error_class <= 0xFF {
        apdu.extend_from_slice(&[0x91, error_class as u8]);
    } else {
        apdu.push(0x92);
        apdu.extend_from_slice(&(error_class as u16).to_be_bytes());
    }
    // error-code (Enumerated)
    if error_code <= 0xFF {
        apdu.extend_from_slice(&[0x91, error_code as u8]);
    } else {
        apdu.push(0x92);
        apdu.extend_from_slice(&(error_code as u16).to_be_bytes());
    }
    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a Simple-ACK PDU (PDU type 0x20).
///
/// Used to acknowledge confirmed services with no result data
/// (e.g. WriteProperty). Mostly used in tests as the mock-device reply.
pub fn encode_simple_ack(invoke_id: u8, service_choice: u8) -> Vec<u8> {
    let apdu: Vec<u8> = vec![
        0x20, // PDU type = Simple-ACK
        invoke_id,
        service_choice,
    ];
    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a WriteProperty-Request (Confirmed-Request) frame.
///
/// # Arguments
/// * `invoke_id`    – 0–255 transaction ID
/// * `object_type`  – BACnet object type (0=AI, 1=AO, 2=AV, …)
/// * `instance`     – Object instance number (22 bits max)
/// * `property_id`  – Property identifier (e.g. 85 = Present_Value)
/// * `value`        – the application-tagged value to write
/// * `array_index`  – optional array index for array properties
/// * `priority`     – optional write priority (1-16, 1=highest, 16=auto-relinquish).
///   Most callers should pass `Some(16)` or `None` (=16 default).
///
/// # Wire layout
/// ```text
/// APDU:
///   0x00              PDU type = Confirmed-Request (no segmentation)
///   0x05              max-segments=0, max-apdu=1476
///   invoke_id
///   0x0F              service-choice = WriteProperty
///   context 0: ObjectIdentifier   (0x0C + 4 bytes)
///   context 1: PropertyIdentifier
///   context 2 (optional): ArrayIndex
///   context 3 opening (0x3E)
///     application-tagged value
///   context 3 closing (0x3F)
///   context 4 (optional): Priority
/// ```
pub fn encode_write_property(
    invoke_id: u8,
    object_type: u16,
    instance: u32,
    property_id: u32,
    value: &super::value::BacnetValue,
    array_index: Option<u32>,
    priority: Option<u8>,
) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x00, // PDU type = Confirmed-Request, no segmentation
        0x05, // max-segments=unspecified (0), max-apdu=1476 (5)
        invoke_id,
        SVC_CONFIRMED_WRITE_PROPERTY,
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

    // Context tag 3: property-value opening (0x3E)
    apdu.push(0x3E);
    encode_bacnet_value(&mut apdu, value);
    // Context tag 3: property-value closing (0x3F)
    apdu.push(0x3F);

    // Context tag 4: Priority (optional)
    if let Some(prio) = priority {
        encode_context_unsigned(&mut apdu, 4, prio as u32);
    }

    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a SubscribeCOV-Request (Confirmed-Request) frame (Phase B8 stub).
///
/// This is a minimal encoder: the parallel frame-agent's authoritative version
/// will replace it and should merge cleanly (same signature, same wire format).
///
/// # Arguments
/// * `invoke_id` — transaction ID
/// * `subscriber_process_id` — unique subscriber-process-identifier (0 is reserved)
/// * `monitored_object_type` — BACnet object type
/// * `monitored_object_instance` — object instance
/// * `issue_confirmed` — `Some(true/false)` subscribes; `None` cancels
/// * `lifetime` — subscription lifetime in seconds; `None` = indefinite
pub fn encode_subscribe_cov(
    invoke_id: u8,
    subscriber_process_id: u32,
    monitored_object_type: u16,
    monitored_object_instance: u32,
    issue_confirmed: Option<bool>,
    lifetime: Option<u32>,
) -> Vec<u8> {
    let mut apdu: Vec<u8> = vec![
        0x00, // PDU type = Confirmed-Request, no segmentation
        0x05, // max-segments=unspecified (0), max-apdu=1476 (5)
        invoke_id,
        SVC_CONFIRMED_SUBSCRIBE_COV,
    ];

    // Context tag 0: subscriber-process-identifier (Unsigned)
    encode_context_unsigned(&mut apdu, 0, subscriber_process_id);

    // Context tag 1: monitored-object-identifier (ObjectIdentifier, 4 bytes)
    let obj_id = ((monitored_object_type as u32) << 22) | (monitored_object_instance & 0x003F_FFFF);
    apdu.push(0x1C); // tag 1, context class (0x08), length 4
    apdu.extend_from_slice(&obj_id.to_be_bytes());

    // Tags 2 and 3 are only present in the SUBSCRIBE form, not the CANCEL form.
    if let Some(confirmed) = issue_confirmed {
        // Context tag 2: issue-confirmed-notifications (Boolean)
        // Tag byte 0x29 = tag 2, context class, length 1; value 0x01/0x00.
        apdu.push(0x29);
        apdu.push(if confirmed { 0x01 } else { 0x00 });

        // Context tag 3: lifetime (Unsigned seconds) — only when Some.
        if let Some(secs) = lifetime {
            encode_context_unsigned(&mut apdu, 3, secs);
        }
    }

    build_packet(BVLL_UNICAST, apdu)
}

/// Encode a single `BacnetValue` as application-tagged bytes.
fn encode_bacnet_value(buf: &mut Vec<u8>, val: &super::value::BacnetValue) {
    use super::value::BacnetValue::*;
    match val {
        Real(f) => {
            buf.push(0x44); // tag 4, LVT=4
            buf.extend_from_slice(&f.to_be_bytes());
        }
        Double(d) => {
            buf.push(0x55); // tag 5, LVT=5 (extended length indicator)
            buf.push(8); // 8 bytes
            buf.extend_from_slice(&d.to_be_bytes());
        }
        Unsigned(n) => {
            if *n <= 0xFF {
                buf.extend_from_slice(&[0x21, *n as u8]);
            } else if *n <= 0xFFFF {
                let b = (*n as u16).to_be_bytes();
                buf.extend_from_slice(&[0x22, b[0], b[1]]);
            } else {
                buf.push(0x24);
                buf.extend_from_slice(&n.to_be_bytes());
            }
        }
        Boolean(b) => {
            buf.push(if *b { 0x11 } else { 0x10 });
        }
        ObjectId {
            object_type,
            instance,
        } => {
            let raw: u32 = ((*object_type as u32) << 22) | (*instance & 0x3F_FFFF);
            buf.push(0xC4); // tag 12, LVT=4
            buf.extend_from_slice(&raw.to_be_bytes());
        }
        Enumerated(v) => {
            if *v <= 0xFF {
                buf.extend_from_slice(&[0x91, *v as u8]);
            } else {
                buf.push(0x92);
                let b = (*v as u16).to_be_bytes();
                buf.extend_from_slice(&b);
            }
        }
        CharacterString(s) => {
            // Tag 7, LVT = byte count (UTF-8 string length + 1 for encoding byte)
            let bytes = s.as_bytes();
            let len = bytes.len() + 1; // +1 for charset byte (0x00 = UTF-8)
            if len <= 4 {
                buf.push(0x70 | len as u8);
            } else if len <= 253 {
                buf.push(0x75); // extended length, 1 byte
                buf.push(len as u8);
            } else {
                buf.push(0x76); // extended length, 2 bytes
                buf.extend_from_slice(&(len as u16).to_be_bytes());
            }
            buf.push(0x00); // charset = UTF-8
            buf.extend_from_slice(bytes);
        }
        Null => buf.push(0x00),
        Array(items) => {
            for item in items {
                encode_bacnet_value(buf, item);
            }
        }
        _ => {} // skip unknown/unhandled types
    }
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
        0x20 => decode_simple_ack(data),
        0x30 => decode_complex_ack(data),
        0x50 => decode_error_pdu(data),
        _ => Ok(Apdu::Other {
            pdu_type: data[0],
            invoke_id: if data.len() > 1 { data[1] } else { 0 },
            data: data.to_vec(),
        }),
    }
}

/// Decode a Simple-ACK PDU (PDU type 0x20).
/// Layout: `[0x20, invoke_id, service_choice]`.
fn decode_simple_ack(data: &[u8]) -> Result<Apdu, BacnetError> {
    if data.len() < 3 {
        return Err(BacnetError::MalformedFrame("Simple-ACK too short".into()));
    }
    Ok(Apdu::SimpleAck {
        invoke_id: data[1],
        service_choice: data[2],
    })
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
        SVC_CONFIRMED_WRITE_PROPERTY => decode_write_property_request(invoke_id, &data[4..]),
        SVC_CONFIRMED_READ_PROPERTY_MULTIPLE => {
            decode_read_property_multiple_request(invoke_id, &data[4..])
        }
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
        SVC_CONFIRMED_READ_PROPERTY_MULTIPLE => {
            decode_read_property_multiple_ack(invoke_id, &data[3..])
        }
        _ => Ok(Apdu::Other {
            pdu_type: data[0],
            invoke_id,
            data: data.to_vec(),
        }),
    }
}

/// Decode a ReadPropertyMultiple-Request payload (Phase B9).
///
/// The payload holds one or more ReadAccessSpecification structures:
///   [0x0C, <objid:4>]            — context tag 0: object-identifier
///   [0x1E]                       — context tag 1 opening: list-of-property-references
///     for each property reference (context tags reset here):
///       context tag 0: property-identifier
///       optional context tag 1: property-array-index
///   [0x1F]                       — context tag 1 closing
fn decode_read_property_multiple_request(
    invoke_id: u8,
    payload: &[u8],
) -> Result<Apdu, BacnetError> {
    let mut pos = 0usize;
    let mut specs: Vec<RpmRequestSpec> = Vec::new();

    while pos < payload.len() {
        // Object-identifier: context tag 0, LVT=4  → 0x0C
        if pos + 5 > payload.len() {
            return Err(BacnetError::MalformedFrame(
                "RPM request: object-identifier truncated".into(),
            ));
        }
        if payload[pos] != 0x0C {
            return Err(BacnetError::MalformedFrame(format!(
                "RPM request: expected context tag 0 (0x0C), got 0x{:02X}",
                payload[pos]
            )));
        }
        let raw_id = u32::from_be_bytes([
            payload[pos + 1],
            payload[pos + 2],
            payload[pos + 3],
            payload[pos + 4],
        ]);
        let object_type = ((raw_id >> 22) & 0x3FF) as u16;
        let instance = raw_id & 0x003F_FFFF;
        pos += 5;

        // list-of-property-references opening: 0x1E
        if pos >= payload.len() || payload[pos] != 0x1E {
            return Err(BacnetError::MalformedFrame(
                "RPM request: expected list-of-property-references opening 0x1E".into(),
            ));
        }
        pos += 1;

        while pos < payload.len() && payload[pos] != 0x1F {
            // Inner context tag 0: property-identifier
            let (prop_id, consumed) = read_context_unsigned(&payload[pos..], 0)?;
            pos += consumed;

            // Optional inner context tag 1: property-array-index
            let mut array_index: Option<u32> = None;
            if pos < payload.len() {
                let tb = payload[pos];
                let tag_num = (tb >> 4) & 0x0F;
                let is_context = (tb & 0x08) != 0;
                if is_context && tag_num == 1 && tb != 0x1E && tb != 0x1F {
                    let (ai, ai_consumed) = read_context_unsigned(&payload[pos..], 1)?;
                    array_index = Some(ai);
                    pos += ai_consumed;
                }
            }

            specs.push(RpmRequestSpec {
                object_type,
                instance,
                property_id: prop_id,
                array_index,
            });
        }

        // list-of-property-references closing: 0x1F
        if pos >= payload.len() || payload[pos] != 0x1F {
            return Err(BacnetError::MalformedFrame(
                "RPM request: expected list-of-property-references closing 0x1F".into(),
            ));
        }
        pos += 1;
    }

    Ok(Apdu::ReadPropertyMultipleRequest { invoke_id, specs })
}

/// Decode a ReadPropertyMultiple-ACK complex-ACK payload (Phase B9).
///
/// The payload holds a sequence of BACnetReadAccessResult structures:
///   [0x0C, <objid:4>]            — context tag 0: object-identifier
///   [0x1E]                       — context tag 1 opening: list-of-results
///     for each property:
///       [context tag 2: property-id]
///       [context tag 3: property-array-index]?
///       either [0x4E, <value>, 0x4F]  — property-value
///       or     [0x5E, <class>, <code>, 0x5F]  — property-access-error
///   [0x1F]                       — context tag 1 closing
fn decode_read_property_multiple_ack(invoke_id: u8, payload: &[u8]) -> Result<Apdu, BacnetError> {
    let mut pos = 0usize;
    let mut results: Vec<RpmResult> = Vec::new();

    while pos < payload.len() {
        // Object-identifier: context tag 0, LVT=4
        if pos + 5 > payload.len() {
            return Err(BacnetError::MalformedFrame(
                "RPM-ACK: object-identifier truncated".into(),
            ));
        }
        if payload[pos] != 0x0C {
            return Err(BacnetError::MalformedFrame(format!(
                "RPM-ACK: expected context tag 0 (0x0C), got 0x{:02X}",
                payload[pos]
            )));
        }
        let raw_id = u32::from_be_bytes([
            payload[pos + 1],
            payload[pos + 2],
            payload[pos + 3],
            payload[pos + 4],
        ]);
        let object_type = ((raw_id >> 22) & 0x3FF) as u16;
        let instance = raw_id & 0x003F_FFFF;
        pos += 5;

        // list-of-results opening: 0x1E
        if pos >= payload.len() || payload[pos] != 0x1E {
            return Err(BacnetError::MalformedFrame(
                "RPM-ACK: expected list-of-results opening tag 0x1E".into(),
            ));
        }
        pos += 1;

        while pos < payload.len() && payload[pos] != 0x1F {
            // Context tag 2: property-identifier
            let (prop_id, consumed) = read_context_unsigned(&payload[pos..], 2)?;
            pos += consumed;

            // Optional context tag 3: property-array-index
            let mut array_index: Option<u32> = None;
            if pos < payload.len() {
                let tb = payload[pos];
                if (tb >> 4) & 0x0F == 3 && (tb & 0x08) != 0 && tb != 0x3E && tb != 0x3F {
                    let (ai, ai_consumed) = read_context_unsigned(&payload[pos..], 3)?;
                    array_index = Some(ai);
                    pos += ai_consumed;
                }
            }

            if pos >= payload.len() {
                return Err(BacnetError::MalformedFrame(
                    "RPM-ACK: truncated after property-id".into(),
                ));
            }

            match payload[pos] {
                0x4E => {
                    // property-value opening
                    pos += 1;
                    // Decode one application-tagged value.
                    let (val, consumed) = decode_application_tag(&payload[pos..])?;
                    pos += consumed;
                    // Expect closing 0x4F.
                    if pos >= payload.len() || payload[pos] != 0x4F {
                        return Err(BacnetError::MalformedFrame(
                            "RPM-ACK: expected property-value closing 0x4F".into(),
                        ));
                    }
                    pos += 1;
                    results.push(RpmResult {
                        object_type,
                        instance,
                        property_id: prop_id,
                        array_index,
                        value: Ok(val),
                    });
                }
                0x5E => {
                    // property-access-error opening
                    pos += 1;
                    let error_class = decode_enumerated_at(payload, &mut pos)?;
                    let error_code = decode_enumerated_at(payload, &mut pos)?;
                    if pos >= payload.len() || payload[pos] != 0x5F {
                        return Err(BacnetError::MalformedFrame(
                            "RPM-ACK: expected property-access-error closing 0x5F".into(),
                        ));
                    }
                    pos += 1;
                    results.push(RpmResult {
                        object_type,
                        instance,
                        property_id: prop_id,
                        array_index,
                        value: Err((error_class, error_code)),
                    });
                }
                tb => {
                    return Err(BacnetError::MalformedFrame(format!(
                        "RPM-ACK: unexpected tag 0x{tb:02X} in result"
                    )));
                }
            }
        }

        // list-of-results closing: 0x1F
        if pos >= payload.len() || payload[pos] != 0x1F {
            return Err(BacnetError::MalformedFrame(
                "RPM-ACK: expected list-of-results closing 0x1F".into(),
            ));
        }
        pos += 1;
    }

    Ok(Apdu::ReadPropertyMultipleAck { invoke_id, results })
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

/// Decode a WriteProperty-Request payload (after invoke_id and service byte).
///
/// Layout mirrors `decode_read_property_request` but additionally parses the
/// `0x3E ... 0x3F` property-value wrapper and an optional context tag 4 (priority).
fn decode_write_property_request(invoke_id: u8, payload: &[u8]) -> Result<Apdu, BacnetError> {
    // Context tag 0: ObjectIdentifier — tag byte 0x0C
    if payload.len() < 5 {
        return Err(BacnetError::MalformedFrame(
            "WriteProperty request: ObjectId truncated".into(),
        ));
    }
    if payload[0] != 0x0C {
        return Err(BacnetError::MalformedFrame(format!(
            "WriteProperty request: expected context tag 0 (0x0C), got 0x{:02X}",
            payload[0]
        )));
    }
    let raw_id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let object_type = ((raw_id >> 22) & 0x3FF) as u16;
    let instance = raw_id & 0x003F_FFFF;
    let mut pos = 5;

    // Context tag 1: PropertyIdentifier
    let (prop_id, consumed) = read_context_unsigned(&payload[pos..], 1)?;
    pos += consumed;

    // Optional context tag 2: ArrayIndex
    let array_index = if pos < payload.len() {
        let tag_byte = payload[pos];
        let tag_num = (tag_byte >> 4) & 0x0F;
        let is_context = (tag_byte & 0x08) != 0;
        if is_context && tag_num == 2 {
            let (idx, consumed) = read_context_unsigned(&payload[pos..], 2)?;
            pos += consumed;
            Some(idx)
        } else {
            None
        }
    } else {
        None
    };

    // Opening tag 3: 0x3E
    if pos >= payload.len() || payload[pos] != 0x3E {
        return Err(BacnetError::MalformedFrame(format!(
            "WriteProperty request: expected opening tag 3 (0x3E) at pos {pos}, got 0x{:02X}",
            if pos < payload.len() { payload[pos] } else { 0 }
        )));
    }
    pos += 1;

    // Application-tagged value (single value between 0x3E and 0x3F)
    if pos >= payload.len() {
        return Err(BacnetError::MalformedFrame(
            "WriteProperty request: value truncated".into(),
        ));
    }
    let (value, consumed) = decode_application_tag(&payload[pos..])?;
    pos += consumed;

    // Closing tag 3: 0x3F
    if pos >= payload.len() || payload[pos] != 0x3F {
        return Err(BacnetError::MalformedFrame(format!(
            "WriteProperty request: expected closing tag 3 (0x3F) at pos {pos}, got 0x{:02X}",
            if pos < payload.len() { payload[pos] } else { 0 }
        )));
    }
    pos += 1;

    // Optional context tag 4: Priority
    let priority = if pos < payload.len() {
        let tag_byte = payload[pos];
        let tag_num = (tag_byte >> 4) & 0x0F;
        let is_context = (tag_byte & 0x08) != 0;
        if is_context && tag_num == 4 {
            let (prio, consumed) = read_context_unsigned(&payload[pos..], 4)?;
            pos += consumed;
            let _ = pos;
            Some(prio as u8)
        } else {
            None
        }
    } else {
        None
    };

    Ok(Apdu::WritePropertyRequest {
        invoke_id,
        object_type,
        instance,
        property_id: prop_id,
        array_index,
        value,
        priority,
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

    // Collect ALL application-tagged values until closing tag 3 (0x3F).
    // For scalar properties this yields exactly 1 item (backward compatible).
    // For array properties (like ObjectList) this yields all items.
    let mut values: Vec<super::value::BacnetValue> = Vec::new();
    while pos < payload.len() && payload[pos] != 0x3F {
        let (val, n) = decode_application_tag(&payload[pos..])?;
        pos += n;
        // Skip Unknown (context-tagged items) — not application values.
        if !matches!(val, super::value::BacnetValue::Unknown) {
            values.push(val);
        }
    }

    // Backward-compatible: single value → scalar; multiple values → Array.
    let value = if values.len() == 1 {
        values.remove(0)
    } else {
        super::value::BacnetValue::Array(values)
    };

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
    fn decode_apdu_simple_ack_decodes_to_simple_ack_variant() {
        // Simple-ACK (0x20) is now decoded to Apdu::SimpleAck.
        let pkt = [0x20, 0x05, 0x0F];
        let apdu = decode_apdu(&pkt).unwrap();
        match apdu {
            Apdu::SimpleAck {
                invoke_id,
                service_choice,
            } => {
                assert_eq!(invoke_id, 0x05);
                assert_eq!(service_choice, 0x0F);
            }
            other => panic!("expected SimpleAck, got {other:?}"),
        }
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

    // ── Phase B4: multi-value ReadPropertyAck tests ─────────────

    /// Three ObjectId values (ObjectList property) round-trip as BacnetValue::Array.
    #[test]
    fn read_property_ack_multi_object_list_round_trip() {
        use bacnet_value::BacnetValue;
        let objects = vec![
            BacnetValue::ObjectId {
                object_type: 0,
                instance: 1,
            }, // AI-1
            BacnetValue::ObjectId {
                object_type: 1,
                instance: 2,
            }, // AO-2
            BacnetValue::ObjectId {
                object_type: 3,
                instance: 0,
            }, // BI-0
        ];
        let frame = encode_read_property_ack_multi(42, 8, 1001, 76, &objects);
        let (_, apdu) = decode_packet(&frame).expect("should decode");
        match apdu {
            Apdu::ReadPropertyAck {
                invoke_id,
                object_type,
                instance,
                property_id,
                value,
            } => {
                assert_eq!(invoke_id, 42);
                assert_eq!(object_type, 8); // Device
                assert_eq!(instance, 1001);
                assert_eq!(property_id, 76); // ObjectList
                match value {
                    BacnetValue::Array(items) => {
                        assert_eq!(items.len(), 3);
                        assert_eq!(
                            items[0],
                            BacnetValue::ObjectId {
                                object_type: 0,
                                instance: 1
                            }
                        );
                        assert_eq!(
                            items[1],
                            BacnetValue::ObjectId {
                                object_type: 1,
                                instance: 2
                            }
                        );
                        assert_eq!(
                            items[2],
                            BacnetValue::ObjectId {
                                object_type: 3,
                                instance: 0
                            }
                        );
                    }
                    other => panic!("expected Array, got {other:?}"),
                }
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    /// Single value encoded via encode_read_property_ack returns scalar, not Array.
    #[test]
    fn read_property_ack_single_value_not_wrapped_in_array() {
        use bacnet_value::BacnetValue;
        let frame = encode_read_property_ack(7, 0, 5, 85, &BacnetValue::Real(22.5));
        let (_, apdu) = decode_packet(&frame).expect("should decode");
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => {
                assert_eq!(
                    value,
                    BacnetValue::Real(22.5),
                    "single value should NOT be wrapped in Array"
                );
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    /// Encoding zero values returns BacnetValue::Array(vec![]).
    #[test]
    fn read_property_ack_empty_array() {
        use bacnet_value::BacnetValue;
        let frame = encode_read_property_ack_multi(1, 8, 100, 76, &[]);
        let (_, apdu) = decode_packet(&frame).expect("should decode");
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => {
                assert_eq!(
                    value,
                    BacnetValue::Array(vec![]),
                    "empty multi should give Array([])"
                );
            }
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    /// Two Real values round-trip as BacnetValue::Array([Real(1.0), Real(2.5)]).
    #[test]
    fn read_property_ack_two_reals() {
        use bacnet_value::BacnetValue;
        let vals = vec![BacnetValue::Real(1.0_f32), BacnetValue::Real(2.5_f32)];
        let frame = encode_read_property_ack_multi(5, 2, 3, 85, &vals);
        let (_, apdu) = decode_packet(&frame).expect("decode");
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => match value {
                BacnetValue::Array(items) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0], BacnetValue::Real(1.0));
                    assert_eq!(items[1], BacnetValue::Real(2.5));
                }
                other => panic!("expected Array, got {other:?}"),
            },
            other => panic!("expected ReadPropertyAck, got {other:?}"),
        }
    }

    /// Single CharacterString via multi encoder returns scalar (not Array).
    #[test]
    fn read_property_ack_char_string_round_trip() {
        use bacnet_value::BacnetValue;
        let vals = vec![BacnetValue::CharacterString("TempSensor".into())];
        let frame = encode_read_property_ack_multi(3, 0, 1, 77, &vals);
        let (_, apdu) = decode_packet(&frame).unwrap();
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => {
                // Single CharacterString — should NOT be wrapped in Array
                assert_eq!(value, BacnetValue::CharacterString("TempSensor".into()));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Three Unsigned values round-trip as BacnetValue::Array.
    #[test]
    fn read_property_ack_multi_unsigned_values() {
        use bacnet_value::BacnetValue;
        let vals = vec![
            BacnetValue::Unsigned(10),
            BacnetValue::Unsigned(20),
            BacnetValue::Unsigned(30),
        ];
        let frame = encode_read_property_ack_multi(99, 2, 7, 85, &vals);
        let (_, apdu) = decode_packet(&frame).unwrap();
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => match value {
                BacnetValue::Array(items) => {
                    assert_eq!(items.len(), 3);
                    assert_eq!(items[0], BacnetValue::Unsigned(10));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// ObjectList with Device itself included encodes/decodes correctly.
    #[test]
    fn encode_read_property_ack_multi_with_device_self() {
        use bacnet_value::BacnetValue;
        let objects = vec![
            BacnetValue::ObjectId {
                object_type: 8,
                instance: 1001,
            }, // Device itself
            BacnetValue::ObjectId {
                object_type: 0,
                instance: 0,
            }, // AI-0
        ];
        let frame = encode_read_property_ack_multi(10, 8, 1001, 76, &objects);
        let (_, apdu) = decode_packet(&frame).unwrap();
        match apdu {
            Apdu::ReadPropertyAck { value, .. } => match value {
                BacnetValue::Array(items) => {
                    assert_eq!(items.len(), 2);
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    // ── Phase B7: WriteProperty + SimpleAck tests ──────────────

    /// WriteProperty with Real(72.5), priority Some(16), no array index.
    /// Byte spot-checks: 0x0F (service), 0x0C (obj-id tag), 0x3E/0x3F wrappers,
    /// 0x44 (Real app tag).
    #[test]
    fn encode_write_property_real_value() {
        use bacnet_value::BacnetValue;
        let pkt = encode_write_property(5, 0, 1, 85, &BacnetValue::Real(72.5), None, Some(16));

        // Scan the APDU portion (after BVLL(4)+NPDU(2)=6).
        let apdu = &pkt[6..];
        assert_eq!(apdu[0], 0x00, "Confirmed-Request PDU type");
        assert_eq!(apdu[1], 0x05, "max-segs/max-apdu");
        assert_eq!(apdu[2], 5, "invoke_id");
        assert_eq!(apdu[3], 0x0F, "service = WriteProperty");
        assert_eq!(apdu[4], 0x0C, "context tag 0 ObjectIdentifier");

        // The full packet must contain the 0x3E/0x3F wrappers and 0x44 (Real).
        assert!(pkt.contains(&0x3E), "opening tag 3 present");
        assert!(pkt.contains(&0x3F), "closing tag 3 present");
        assert!(pkt.contains(&0x44), "Real app-tag present");

        // Priority 16 → context tag 4, LVT=1, value 0x10 → 0x49 0x10.
        let has_prio = pkt.windows(2).any(|w| w == [0x49, 0x10]);
        assert!(has_prio, "priority context tag 4 (0x49 0x10) present");
    }

    /// No priority: must NOT contain the 0x49 context-tag-4 byte after the
    /// closing tag 3.
    #[test]
    fn encode_write_property_no_priority() {
        use bacnet_value::BacnetValue;
        let pkt = encode_write_property(5, 0, 1, 85, &BacnetValue::Real(10.0), None, None);
        let close_idx = pkt
            .iter()
            .rposition(|&b| b == 0x3F)
            .expect("closing tag 3 present");
        let tail = &pkt[close_idx + 1..];
        assert!(
            !tail.contains(&0x49),
            "no priority context tag 4 should appear after closing tag"
        );
    }

    /// With array_index=Some(1) → context tag 2, LVT=1, value 1 → 0x29 0x01
    /// must appear before the 0x3E opening tag.
    #[test]
    fn encode_write_property_with_array_index() {
        use bacnet_value::BacnetValue;
        let pkt = encode_write_property(5, 0, 1, 85, &BacnetValue::Real(1.0), Some(1), Some(16));
        let open_idx = pkt
            .iter()
            .position(|&b| b == 0x3E)
            .expect("opening tag 3 present");
        let prefix = &pkt[..open_idx];
        let has_array = prefix.windows(2).any(|w| w == [0x29, 0x01]);
        assert!(has_array, "context tag 2 (0x29 0x01) must precede 0x3E");
    }

    /// Round-trip: encode then decode_packet → WritePropertyRequest matches.
    #[test]
    fn write_property_request_round_trip() {
        use bacnet_value::BacnetValue;
        let pkt = encode_write_property(9, 1, 5, 85, &BacnetValue::Real(23.5), None, Some(16));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::WritePropertyRequest {
                invoke_id: 9,
                object_type: 1,
                instance: 5,
                property_id: 85,
                array_index: None,
                value: BacnetValue::Real(23.5),
                priority: Some(16),
            }
        );
    }

    /// Boolean round-trip — BO object with Boolean(true).
    #[test]
    fn write_property_request_boolean_round_trip() {
        use bacnet_value::BacnetValue;
        let pkt = encode_write_property(3, 4, 2, 85, &BacnetValue::Boolean(true), None, Some(16));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::WritePropertyRequest {
                invoke_id,
                object_type,
                instance,
                property_id,
                value,
                priority,
                array_index,
            } => {
                assert_eq!(invoke_id, 3);
                assert_eq!(object_type, 4);
                assert_eq!(instance, 2);
                assert_eq!(property_id, 85);
                assert_eq!(value, BacnetValue::Boolean(true));
                assert_eq!(priority, Some(16));
                assert_eq!(array_index, None);
            }
            other => panic!("expected WritePropertyRequest, got {other:?}"),
        }
    }

    /// Bare Simple-ACK frame decodes to Apdu::SimpleAck.
    #[test]
    fn simple_ack_decode() {
        let pkt = [
            0x81, 0x0A, 0x00, 0x09, // BVLL unicast, length 9
            0x01, 0x00, // NPDU version, control
            0x20, 0x42, 0x0F, // PDU type SimpleAck, invoke_id 0x42, svc 0x0F
        ];
        let (_, apdu) = decode_packet(&pkt).unwrap();
        assert_eq!(
            apdu,
            Apdu::SimpleAck {
                invoke_id: 0x42,
                service_choice: 0x0F,
            }
        );
    }

    /// Truncated Simple-ACK (missing invoke_id/service) returns Err.
    #[test]
    fn simple_ack_truncated_returns_error() {
        let pkt = [
            0x81, 0x0A, 0x00, 0x07, // BVLL unicast, length 7
            0x01, 0x00, // NPDU
            0x20, // PDU type only
        ];
        assert!(decode_packet(&pkt).is_err());
    }

    /// Unsigned round-trip — value 100 encoded and decoded cleanly.
    #[test]
    fn encode_write_property_unsigned() {
        use bacnet_value::BacnetValue;
        let pkt = encode_write_property(4, 2, 7, 85, &BacnetValue::Unsigned(100), None, Some(16));
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::WritePropertyRequest { value, .. } => {
                assert_eq!(value, BacnetValue::Unsigned(100));
            }
            other => panic!("expected WritePropertyRequest, got {other:?}"),
        }
    }

    /// Hand-computed golden wire layout for a specific WriteProperty:
    /// invoke_id=7, AO (1), instance 5, property 85, Real(50.0), priority 16.
    ///
    /// This pins the byte layout so any future encoder change is flagged.
    #[test]
    fn encode_write_property_wire_snapshot() {
        use bacnet_value::BacnetValue;
        let pkt = encode_write_property(7, 1, 5, 85, &BacnetValue::Real(50.0), None, Some(16));

        // Build the expected APDU manually.
        // 0x00 0x05 0x07 0x0F  — Confirmed-Req, max-seg/apdu, invoke, svc WriteProp
        // 0x0C <4-byte obj-id> — context tag 0: (AO=1 << 22) | 5 = 0x00400005
        // 0x19 0x55            — context tag 1 LVT=1, property 85
        // 0x3E                 — opening tag 3
        // 0x44 <4-byte Real>   — app tag 4, LVT=4, Real(50.0) big-endian
        // 0x3F                 — closing tag 3
        // 0x49 0x10            — context tag 4 LVT=1, priority 16
        let real_bytes = 50.0f32.to_be_bytes();
        let obj_id: u32 = (1u32 << 22) | 5;
        let obj_bytes = obj_id.to_be_bytes();
        let mut expected_apdu: Vec<u8> = vec![0x00, 0x05, 0x07, 0x0F, 0x0C];
        expected_apdu.extend_from_slice(&obj_bytes);
        expected_apdu.extend_from_slice(&[0x19, 0x55, 0x3E, 0x44]);
        expected_apdu.extend_from_slice(&real_bytes);
        expected_apdu.extend_from_slice(&[0x3F, 0x49, 0x10]);

        // Wrap with BVLL + NPDU (6 bytes header)
        let total_len = 6 + expected_apdu.len();
        let mut expected: Vec<u8> = Vec::with_capacity(total_len);
        expected.push(0x81);
        expected.push(BVLL_UNICAST);
        expected.push((total_len >> 8) as u8);
        expected.push((total_len & 0xFF) as u8);
        expected.push(0x01);
        expected.push(0x00);
        expected.extend_from_slice(&expected_apdu);

        assert_eq!(pkt, expected, "WriteProperty wire layout golden mismatch");
    }

    // ── Phase B9: ReadPropertyMultiple tests ────────────────────

    /// RPM request with a single spec round-trips cleanly.
    #[test]
    fn rpm_request_single_spec_round_trip() {
        let specs = vec![RpmRequestSpec {
            object_type: 0, // AI
            instance: 0,
            property_id: 85, // PresentValue
            array_index: None,
        }];
        let pkt = encode_read_property_multiple(5, &specs);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleRequest {
                invoke_id,
                specs: got,
            } => {
                assert_eq!(invoke_id, 5);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].object_type, 0);
                assert_eq!(got[0].instance, 0);
                assert_eq!(got[0].property_id, 85);
                assert_eq!(got[0].array_index, None);
            }
            other => panic!("expected ReadPropertyMultipleRequest, got {other:?}"),
        }
    }

    /// RPM request with three specs for different objects preserves order.
    #[test]
    fn rpm_request_multiple_specs_round_trip() {
        let specs = vec![
            RpmRequestSpec {
                object_type: 0, // AI
                instance: 0,
                property_id: 85,
                array_index: None,
            },
            RpmRequestSpec {
                object_type: 1, // AO
                instance: 5,
                property_id: 85,
                array_index: None,
            },
            RpmRequestSpec {
                object_type: 3, // BI
                instance: 2,
                property_id: 85,
                array_index: None,
            },
        ];
        let pkt = encode_read_property_multiple(0x10, &specs);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleRequest {
                invoke_id,
                specs: got,
            } => {
                assert_eq!(invoke_id, 0x10);
                assert_eq!(got.len(), 3);
                assert_eq!(got[0].object_type, 0);
                assert_eq!(got[0].instance, 0);
                assert_eq!(got[1].object_type, 1);
                assert_eq!(got[1].instance, 5);
                assert_eq!(got[2].object_type, 3);
                assert_eq!(got[2].instance, 2);
                for s in &got {
                    assert_eq!(s.property_id, 85);
                    assert_eq!(s.array_index, None);
                }
            }
            other => panic!("expected ReadPropertyMultipleRequest, got {other:?}"),
        }
    }

    /// RPM request with a property-array-index is preserved round-trip.
    #[test]
    fn rpm_request_with_array_index() {
        let specs = vec![RpmRequestSpec {
            object_type: 8, // Device
            instance: 1001,
            property_id: 76, // ObjectList
            array_index: Some(0),
        }];
        let pkt = encode_read_property_multiple(3, &specs);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleRequest { specs: got, .. } => {
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].array_index, Some(0));
                assert_eq!(got[0].property_id, 76);
            }
            other => panic!("expected ReadPropertyMultipleRequest, got {other:?}"),
        }
    }

    /// RPM request with a 2-byte property identifier round-trips.
    #[test]
    fn rpm_request_two_byte_property_id() {
        let specs = vec![RpmRequestSpec {
            object_type: 0,
            instance: 0,
            property_id: 512, // forces 2-byte encoding
            array_index: None,
        }];
        let pkt = encode_read_property_multiple(1, &specs);
        // Verify the wire really used the 2-byte form (0x0A = context 0 LVT=2).
        assert!(
            pkt.iter().any(|&b| b == 0x0A),
            "expected context tag 0 LVT=2 byte (0x0A) in wire bytes"
        );
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleRequest { specs: got, .. } => {
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].property_id, 512);
            }
            other => panic!("expected ReadPropertyMultipleRequest, got {other:?}"),
        }
    }

    /// RPM ACK with a single Real value round-trips.
    #[test]
    fn rpm_ack_single_real_value_round_trip() {
        let results = vec![RpmResult {
            object_type: 0,
            instance: 1,
            property_id: 85,
            array_index: None,
            value: Ok(BacnetValue::Real(23.5)),
        }];
        let pkt = encode_read_property_multiple_ack(9, &results);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleAck {
                invoke_id,
                results: got,
            } => {
                assert_eq!(invoke_id, 9);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].object_type, 0);
                assert_eq!(got[0].instance, 1);
                assert_eq!(got[0].property_id, 85);
                assert_eq!(got[0].value, Ok(BacnetValue::Real(23.5)));
            }
            other => panic!("expected ReadPropertyMultipleAck, got {other:?}"),
        }
    }

    /// RPM ACK with three heterogeneous results round-trips and preserves order.
    #[test]
    fn rpm_ack_multiple_results() {
        let results = vec![
            RpmResult {
                object_type: 0,
                instance: 0,
                property_id: 85,
                array_index: None,
                value: Ok(BacnetValue::Real(10.0)),
            },
            RpmResult {
                object_type: 1,
                instance: 1,
                property_id: 85,
                array_index: None,
                value: Ok(BacnetValue::Real(20.0)),
            },
            RpmResult {
                object_type: 3,
                instance: 0,
                property_id: 85,
                array_index: None,
                value: Ok(BacnetValue::Enumerated(1)),
            },
        ];
        let pkt = encode_read_property_multiple_ack(0x20, &results);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleAck {
                invoke_id,
                results: got,
            } => {
                assert_eq!(invoke_id, 0x20);
                assert_eq!(got.len(), 3);
                assert_eq!(got[0].value, Ok(BacnetValue::Real(10.0)));
                assert_eq!(got[0].object_type, 0);
                assert_eq!(got[1].value, Ok(BacnetValue::Real(20.0)));
                assert_eq!(got[1].object_type, 1);
                assert_eq!(got[2].value, Ok(BacnetValue::Enumerated(1)));
                assert_eq!(got[2].object_type, 3);
            }
            other => panic!("expected ReadPropertyMultipleAck, got {other:?}"),
        }
    }

    /// RPM ACK with an error result (property-access-error) round-trips.
    #[test]
    fn rpm_ack_with_error_result() {
        let results = vec![RpmResult {
            object_type: 0,
            instance: 0,
            property_id: 85,
            array_index: None,
            value: Err((2, 31)), // object class, write-access-denied code
        }];
        let pkt = encode_read_property_multiple_ack(1, &results);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleAck { results: got, .. } => {
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].value, Err((2, 31)));
            }
            other => panic!("expected ReadPropertyMultipleAck, got {other:?}"),
        }
    }

    /// RPM ACK mixing Ok and Err results preserves ordering and types.
    #[test]
    fn rpm_ack_mixed_ok_and_err() {
        let results = vec![
            RpmResult {
                object_type: 0,
                instance: 0,
                property_id: 85,
                array_index: None,
                value: Ok(BacnetValue::Real(10.0)),
            },
            RpmResult {
                object_type: 1,
                instance: 2,
                property_id: 85,
                array_index: None,
                value: Err((2, 32)),
            },
        ];
        let pkt = encode_read_property_multiple_ack(7, &results);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleAck { results: got, .. } => {
                assert_eq!(got.len(), 2);
                assert_eq!(got[0].value, Ok(BacnetValue::Real(10.0)));
                assert_eq!(got[0].object_type, 0);
                assert_eq!(got[1].value, Err((2, 32)));
                assert_eq!(got[1].object_type, 1);
            }
            other => panic!("expected ReadPropertyMultipleAck, got {other:?}"),
        }
    }

    /// Hand-computed golden wire layout for an RPM request:
    /// invoke_id=0x42, 2 specs: (AI-0 prop 85), (AO-5 prop 85).
    ///
    /// APDU layout:
    ///   0x00 0x05 0x42 0x0E           PDU type, max-segs, invoke_id, service
    ///   0x0C 0x00 0x00 0x00 0x00      ctx tag 0 object-id AI-0
    ///   0x1E                          ctx tag 1 opening
    ///   0x09 0x55                     inner ctx tag 0 LVT=1, property 85
    ///   0x1F                          ctx tag 1 closing
    ///   0x0C 0x00 0x40 0x00 0x05      ctx tag 0 object-id AO-5
    ///   0x1E
    ///   0x09 0x55
    ///   0x1F
    #[test]
    fn rpm_request_wire_snapshot() {
        let specs = vec![
            RpmRequestSpec {
                object_type: 0,
                instance: 0,
                property_id: 85,
                array_index: None,
            },
            RpmRequestSpec {
                object_type: 1,
                instance: 5,
                property_id: 85,
                array_index: None,
            },
        ];
        let pkt = encode_read_property_multiple(0x42, &specs);

        #[rustfmt::skip]
        let expected_apdu: Vec<u8> = vec![
            0x00, 0x05, 0x42, 0x0E,
            0x0C, 0x00, 0x00, 0x00, 0x00,
            0x1E,
            0x09, 0x55,
            0x1F,
            0x0C, 0x00, 0x40, 0x00, 0x05,
            0x1E,
            0x09, 0x55,
            0x1F,
        ];

        let total_len = 6 + expected_apdu.len();
        let mut expected: Vec<u8> = Vec::with_capacity(total_len);
        expected.push(0x81);
        expected.push(BVLL_UNICAST);
        expected.push((total_len >> 8) as u8);
        expected.push((total_len & 0xFF) as u8);
        expected.push(0x01);
        expected.push(0x00);
        expected.extend_from_slice(&expected_apdu);

        assert_eq!(pkt, expected, "RPM request wire layout golden mismatch");
    }

    /// Hand-computed golden wire layout for an RPM ACK:
    /// invoke_id=0x42, 2 results: AI-0 prop 85 Real(50.0), AO-5 prop 85 Real(75.5).
    ///
    /// APDU layout:
    ///   0x30 0x42 0x0E                PDU type, invoke_id, service
    ///   0x0C 0x00 0x00 0x00 0x00      ctx tag 0 object-id AI-0
    ///   0x1E                          ctx tag 1 opening
    ///   0x29 0x55                     ctx tag 2 LVT=1, property 85
    ///   0x4E                          ctx tag 4 opening (property-value)
    ///   0x44 <4 bytes Real(50.0)>     app tag 4 LVT=4, Real
    ///   0x4F                          ctx tag 4 closing
    ///   0x1F                          ctx tag 1 closing
    ///   (same for AO-5 Real(75.5))
    #[test]
    fn rpm_ack_wire_snapshot() {
        let results = vec![
            RpmResult {
                object_type: 0,
                instance: 0,
                property_id: 85,
                array_index: None,
                value: Ok(BacnetValue::Real(50.0)),
            },
            RpmResult {
                object_type: 1,
                instance: 5,
                property_id: 85,
                array_index: None,
                value: Ok(BacnetValue::Real(75.5)),
            },
        ];
        let pkt = encode_read_property_multiple_ack(0x42, &results);

        let real50 = 50.0f32.to_be_bytes();
        let real75 = 75.5f32.to_be_bytes();

        let mut expected_apdu: Vec<u8> = vec![
            0x30, 0x42, 0x0E, // Complex-ACK, invoke_id, service
            0x0C, 0x00, 0x00, 0x00, 0x00, // object-id AI-0
            0x1E, // list-of-results opening
            0x29, 0x55, // ctx 2 LVT=1 property 85
            0x4E, // value opening
            0x44,
        ];
        expected_apdu.extend_from_slice(&real50);
        expected_apdu.extend_from_slice(&[0x4F, 0x1F]); // value closing, list closing
        expected_apdu
            .extend_from_slice(&[0x0C, 0x00, 0x40, 0x00, 0x05, 0x1E, 0x29, 0x55, 0x4E, 0x44]);
        expected_apdu.extend_from_slice(&real75);
        expected_apdu.extend_from_slice(&[0x4F, 0x1F]);

        let total_len = 6 + expected_apdu.len();
        let mut expected: Vec<u8> = Vec::with_capacity(total_len);
        expected.push(0x81);
        expected.push(BVLL_UNICAST);
        expected.push((total_len >> 8) as u8);
        expected.push((total_len & 0xFF) as u8);
        expected.push(0x01);
        expected.push(0x00);
        expected.extend_from_slice(&expected_apdu);

        assert_eq!(pkt, expected, "RPM ACK wire layout golden mismatch");
    }

    /// RPM request for object-name property (77 = 0x4D) round-trips.
    #[test]
    fn rpm_request_char_string_property() {
        let specs = vec![RpmRequestSpec {
            object_type: 0,
            instance: 1,
            property_id: 77, // ObjectName
            array_index: None,
        }];
        let pkt = encode_read_property_multiple(2, &specs);
        let (_, apdu) = decode_packet(&pkt).unwrap();
        match apdu {
            Apdu::ReadPropertyMultipleRequest { specs: got, .. } => {
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].property_id, 77);
            }
            other => panic!("expected ReadPropertyMultipleRequest, got {other:?}"),
        }
    }

    /// A truncated RPM ACK (missing closing 0x1F) must return MalformedFrame.
    #[test]
    fn decode_rpm_ack_truncated_returns_error() {
        // Start with a valid ACK and strip the trailing list-of-results closing.
        let results = vec![RpmResult {
            object_type: 0,
            instance: 0,
            property_id: 85,
            array_index: None,
            value: Ok(BacnetValue::Real(1.0)),
        }];
        let pkt = encode_read_property_multiple_ack(1, &results);
        // Sanity: last byte of APDU is the 0x1F list-of-results close.
        assert_eq!(*pkt.last().unwrap(), 0x1F);

        // Build a truncated copy: drop the final 0x1F and patch BVLL length.
        let mut truncated = pkt.clone();
        truncated.pop();
        let new_len = truncated.len() as u16;
        truncated[2] = (new_len >> 8) as u8;
        truncated[3] = (new_len & 0xFF) as u8;

        match decode_packet(&truncated) {
            Err(BacnetError::MalformedFrame(_)) => {}
            other => panic!("expected MalformedFrame error, got {other:?}"),
        }
    }
}

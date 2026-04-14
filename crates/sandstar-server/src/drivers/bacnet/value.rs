//! BACnet application-tag value encoding and decoding.
//!
//! Handles the primitive value types used in ReadProperty-ACK responses:
//! Real, Double, Unsigned, Signed, Boolean, Enumerated, CharString,
//! ObjectId, Null.
//!
//! # Tag format (1 byte)
//!
//! ```text
//!   bits 7-4: tag number (0=Null, 1=Bool, 2=Uint, 3=Int, 4=Real, 5=Double,
//!             6=OctetStr, 7=CharStr, 8=BitStr, 9=Enum, 10=Date, 11=Time,
//!             12=ObjectId)
//!   bit 3:    0=application, 1=context
//!   bits 2-0: length/value/type (LVT)
//!     - LVT 0-4: value is the LVT bytes (for Null, Bool, short values)
//!     - LVT 5:   next byte is extended length
//!     - LVT 6:   next 2 bytes are extended length
//!     - LVT 7:   next 4 bytes are extended length
//! ```
//!
//! Only application-class tags are decoded here. Context tags are returned
//! as [`BacnetValue::Unknown`] so the caller can skip them cleanly.

use super::BacnetError;

// ── BacnetValue ────────────────────────────────────────────────────────────

/// A decoded BACnet application-tagged value.
#[derive(Debug, Clone, PartialEq)]
pub enum BacnetValue {
    /// Application tag 0 — NULL.
    Null,
    /// Application tag 1 — BOOLEAN.
    Boolean(bool),
    /// Application tag 2 — Unsigned Integer (up to 32-bit).
    Unsigned(u32),
    /// Application tag 3 — Signed Integer (up to 32-bit).
    Signed(i32),
    /// Application tag 4 — Real (single-precision IEEE 754 float).
    Real(f32),
    /// Application tag 5 — Double (double-precision IEEE 754 float).
    Double(f64),
    /// Application tag 6 — Octet String.
    OctetString(Vec<u8>),
    /// Application tag 7 — Character String (UTF-8).
    CharacterString(String),
    /// Application tag 8 — Bit String.
    BitString(Vec<u8>),
    /// Application tag 9 — Enumerated.
    Enumerated(u32),
    /// Application tag 10 — Date (4 bytes: year-1900, month, day, day-of-week).
    Date([u8; 4]),
    /// Application tag 11 — Time (4 bytes: hour, minute, second, centiseconds).
    Time([u8; 4]),
    /// Application tag 12 — BACnetObjectIdentifier.
    ObjectId {
        /// BACnet object type number (top 10 bits of the 4-byte encoding).
        object_type: u16,
        /// Object instance number (bottom 22 bits of the 4-byte encoding).
        instance: u32,
    },
    /// Any other / unrecognised tag — skipped cleanly by the decoder.
    Unknown,
    /// A sequence of values (used for array properties like ObjectList).
    Array(Vec<BacnetValue>),
}

impl BacnetValue {
    /// Convert to `f64` for use in `sync_cur` results.
    ///
    /// Returns `None` for non-numeric variants ([`BacnetValue::Null`],
    /// [`BacnetValue::CharacterString`], [`BacnetValue::OctetString`],
    /// [`BacnetValue::BitString`], [`BacnetValue::Date`],
    /// [`BacnetValue::Time`], [`BacnetValue::ObjectId`],
    /// [`BacnetValue::Unknown`]).
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            BacnetValue::Real(v) => Some(*v as f64),
            BacnetValue::Double(v) => Some(*v),
            BacnetValue::Unsigned(v) => Some(*v as f64),
            BacnetValue::Signed(v) => Some(*v as f64),
            BacnetValue::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
            BacnetValue::Enumerated(v) => Some(*v as f64),
            BacnetValue::Array(items) => {
                // For array properties used numerically, try the first element.
                items.first().and_then(|v| v.to_f64())
            }
            _ => None,
        }
    }
}

// ── decode_application_tag ─────────────────────────────────────────────────

/// Decode one application-tagged value from `data`, returning
/// `(value, bytes_consumed)`.
///
/// Returns [`BacnetError::MalformedFrame`] if the slice is too short or
/// the encoding is inconsistent.
///
/// Context-tagged bytes (class bit = 1) are returned as
/// [`BacnetValue::Unknown`] with `bytes_consumed = 1 + LVT` so the caller
/// can skip them cleanly.
pub fn decode_application_tag(data: &[u8]) -> Result<(BacnetValue, usize), BacnetError> {
    if data.is_empty() {
        return Err(BacnetError::MalformedFrame("empty application tag".into()));
    }

    let tag_byte = data[0];
    let tag_num = (tag_byte >> 4) & 0x0F;
    let class = (tag_byte >> 3) & 0x01; // 0 = application, 1 = context
    let lvt = (tag_byte & 0x07) as usize;

    if class != 0 {
        // Context-tagged: LVT is the byte count; skip so callers can advance.
        let consumed = 1 + lvt;
        if data.len() < consumed {
            return Err(BacnetError::MalformedFrame("context tag truncated".into()));
        }
        return Ok((BacnetValue::Unknown, consumed));
    }

    match tag_num {
        // NULL — 0 value bytes
        0 => Ok((BacnetValue::Null, 1)),

        // BOOLEAN — value encoded in LVT itself, no following bytes
        1 => Ok((BacnetValue::Boolean(lvt != 0), 1)),

        // UNSIGNED INTEGER
        2 => {
            let val = read_unsigned(&data[1..], lvt)?;
            Ok((BacnetValue::Unsigned(val), 1 + lvt))
        }

        // SIGNED INTEGER
        3 => {
            let raw = read_unsigned(&data[1..], lvt)?;
            let signed = match lvt {
                1 => raw as i8 as i32,
                2 => raw as i16 as i32,
                3 | 4 => raw as i32,
                _ => raw as i32,
            };
            Ok((BacnetValue::Signed(signed), 1 + lvt))
        }

        // REAL (4 bytes, IEEE 754 single-precision)
        4 => {
            if lvt != 4 {
                return Err(BacnetError::MalformedFrame(
                    "Real tag must have LVT=4".into(),
                ));
            }
            if data.len() < 5 {
                return Err(BacnetError::MalformedFrame("Real value truncated".into()));
            }
            let bytes = [data[1], data[2], data[3], data[4]];
            Ok((BacnetValue::Real(f32::from_be_bytes(bytes)), 5))
        }

        // DOUBLE (8 bytes, IEEE 754 double-precision)
        //
        // The Double tag (tag number 5) carries a fixed 8-byte IEEE-754
        // double-precision value immediately following the tag byte (9 bytes
        // total). The LVT field is treated as a don't-care for this tag.
        5 => {
            if data.len() < 9 {
                return Err(BacnetError::MalformedFrame("Double value truncated".into()));
            }
            let bytes: [u8; 8] = data[1..9].try_into().unwrap();
            Ok((BacnetValue::Double(f64::from_be_bytes(bytes)), 9))
        }

        // OCTET STRING
        6 => {
            if data.len() < 1 + lvt {
                return Err(BacnetError::MalformedFrame("OctetString truncated".into()));
            }
            Ok((BacnetValue::OctetString(data[1..1 + lvt].to_vec()), 1 + lvt))
        }

        // CHARACTER STRING — first value byte is encoding (0 = UTF-8), rest is string
        //
        // LVT 0-4: direct byte count (header = 1 byte total overhead).
        // LVT 5:   next byte holds actual length (2 bytes overhead).
        // LVT 6:   next 2 bytes (BE u16) hold actual length (3 bytes overhead).
        7 => {
            let (str_len, header_len) = match lvt {
                0..=4 => (lvt, 1usize),
                5 => {
                    if data.len() < 2 {
                        return Err(BacnetError::MalformedFrame(
                            "CharacterString extended length truncated".into(),
                        ));
                    }
                    (data[1] as usize, 2)
                }
                6 => {
                    if data.len() < 3 {
                        return Err(BacnetError::MalformedFrame(
                            "CharacterString extended length truncated".into(),
                        ));
                    }
                    (u16::from_be_bytes([data[1], data[2]]) as usize, 3)
                }
                _ => {
                    return Err(BacnetError::MalformedFrame(
                        "CharacterString LVT=7 not supported".into(),
                    ))
                }
            };
            if str_len == 0 {
                return Ok((BacnetValue::CharacterString(String::new()), header_len));
            }
            if data.len() < header_len + str_len {
                return Err(BacnetError::MalformedFrame(
                    "CharacterString truncated".into(),
                ));
            }
            // First byte of string data is the encoding byte (0 = UTF-8)
            let s = if str_len > 1 {
                String::from_utf8_lossy(&data[header_len + 1..header_len + str_len]).into_owned()
            } else {
                String::new()
            };
            Ok((BacnetValue::CharacterString(s), header_len + str_len))
        }

        // BIT STRING
        8 => {
            if data.len() < 1 + lvt {
                return Err(BacnetError::MalformedFrame("BitString truncated".into()));
            }
            Ok((BacnetValue::BitString(data[1..1 + lvt].to_vec()), 1 + lvt))
        }

        // ENUMERATED
        9 => {
            let val = read_unsigned(&data[1..], lvt)?;
            Ok((BacnetValue::Enumerated(val), 1 + lvt))
        }

        // DATE (4 bytes)
        10 => {
            if lvt != 4 || data.len() < 5 {
                return Err(BacnetError::MalformedFrame("Date tag invalid".into()));
            }
            Ok((BacnetValue::Date([data[1], data[2], data[3], data[4]]), 5))
        }

        // TIME (4 bytes)
        11 => {
            if lvt != 4 || data.len() < 5 {
                return Err(BacnetError::MalformedFrame("Time tag invalid".into()));
            }
            Ok((BacnetValue::Time([data[1], data[2], data[3], data[4]]), 5))
        }

        // OBJECT IDENTIFIER (4 bytes: bits 31-22 = object_type, bits 21-0 = instance)
        12 => {
            if lvt != 4 {
                return Err(BacnetError::MalformedFrame(
                    "ObjectId tag must have LVT=4".into(),
                ));
            }
            if data.len() < 5 {
                return Err(BacnetError::MalformedFrame(
                    "ObjectId value truncated".into(),
                ));
            }
            let raw = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
            let object_type = ((raw >> 22) & 0x3FF) as u16;
            let instance = raw & 0x003F_FFFF;
            Ok((
                BacnetValue::ObjectId {
                    object_type,
                    instance,
                },
                5,
            ))
        }

        // Unknown / reserved — skip
        _ => {
            let consumed = 1 + lvt;
            if data.len() < consumed {
                return Err(BacnetError::MalformedFrame(
                    "unknown application tag truncated".into(),
                ));
            }
            Ok((BacnetValue::Unknown, consumed))
        }
    }
}

// ── Encoding helpers ───────────────────────────────────────────────────────

/// Encode a BACnet Unsigned integer with application tag 2.
///
/// Chooses the minimum byte width needed to represent `n`:
/// - 1 byte for values 0–255
/// - 2 bytes for values 256–65535
/// - 3 bytes for values 65536–16777215
/// - 4 bytes for all larger values
///
/// Wire format: `tag_byte | payload_bytes`
/// where `tag_byte = 0x20 | len` (tag=2, class=app, LVT=len).
pub fn encode_unsigned(n: u32) -> Vec<u8> {
    if n <= 0xFF {
        vec![0x21, n as u8]
    } else if n <= 0xFFFF {
        let b = (n as u16).to_be_bytes();
        vec![0x22, b[0], b[1]]
    } else if n <= 0x00FF_FFFF {
        vec![0x23, (n >> 16) as u8, (n >> 8) as u8, n as u8]
    } else {
        let b = n.to_be_bytes();
        vec![0x24, b[0], b[1], b[2], b[3]]
    }
}

/// Encode a BACnet context-tagged object identifier.
///
/// The object identifier is encoded as 4 bytes where the top 10 bits
/// (`bits 31-22`) carry `obj_type` and the bottom 22 bits carry `instance`.
///
/// Context tag byte: `(tag_number << 4) | 0x08 | 0x04`
///   - `tag_number << 4`: places the tag in the upper nibble
///   - `0x08`: sets the class bit (context-specific)
///   - `0x04`: LVT = 4 (4 value bytes follow)
pub fn encode_object_id_context(tag: u8, obj_type: u16, instance: u32) -> Vec<u8> {
    let tag_byte = ((tag & 0x0F) << 4) | 0x08 | 0x04;
    let raw = ((obj_type as u32 & 0x3FF) << 22) | (instance & 0x003F_FFFF);
    let b = raw.to_be_bytes();
    vec![tag_byte, b[0], b[1], b[2], b[3]]
}

/// Encode a BACnet context-tagged property identifier.
///
/// Uses the minimum number of bytes needed to represent the `property_id`
/// value, using the same length rules as [`encode_unsigned`] but with
/// the context class bit set.
///
/// Context tag byte: `(tag_number << 4) | 0x08 | len`
///   - `tag_number << 4`: places the tag in the upper nibble
///   - `0x08`: sets the class bit (context-specific)
///   - `len`: number of value bytes (1–4)
pub fn encode_property_id_context(tag: u8, property_id: u32) -> Vec<u8> {
    let base = ((tag & 0x0F) << 4) | 0x08;
    if property_id <= 0xFF {
        vec![base | 0x01, property_id as u8]
    } else if property_id <= 0xFFFF {
        let b = (property_id as u16).to_be_bytes();
        vec![base | 0x02, b[0], b[1]]
    } else if property_id <= 0x00FF_FFFF {
        vec![
            base | 0x03,
            (property_id >> 16) as u8,
            (property_id >> 8) as u8,
            property_id as u8,
        ]
    } else {
        let b = property_id.to_be_bytes();
        vec![base | 0x04, b[0], b[1], b[2], b[3]]
    }
}

// ── Private helpers ────────────────────────────────────────────────────────

/// Read `len` bytes from `data` as a big-endian unsigned integer (0–4 bytes).
fn read_unsigned(data: &[u8], len: usize) -> Result<u32, BacnetError> {
    if data.len() < len {
        return Err(BacnetError::MalformedFrame(
            "unsigned value truncated".into(),
        ));
    }
    let val = match len {
        0 => 0u32,
        1 => data[0] as u32,
        2 => u16::from_be_bytes([data[0], data[1]]) as u32,
        3 => ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | (data[2] as u32),
        4 => u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        _ => {
            return Err(BacnetError::MalformedFrame(
                "unsigned value too wide".into(),
            ))
        }
    };
    Ok(val)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Null ────────────────────────────────────────────────────────────────

    #[test]
    fn decode_null() {
        let (v, n) = decode_application_tag(&[0x00]).unwrap();
        assert_eq!(v, BacnetValue::Null);
        assert_eq!(n, 1);
    }

    // ── Boolean ─────────────────────────────────────────────────────────────

    #[test]
    fn decode_bool_true() {
        // tag=1, class=app, LVT=1 → true
        let (v, n) = decode_application_tag(&[0x11]).unwrap();
        assert_eq!(v, BacnetValue::Boolean(true));
        assert_eq!(n, 1);
    }

    #[test]
    fn decode_bool_false() {
        // tag=1, class=app, LVT=0 → false
        let (v, n) = decode_application_tag(&[0x10]).unwrap();
        assert_eq!(v, BacnetValue::Boolean(false));
        assert_eq!(n, 1);
    }

    // ── Unsigned ────────────────────────────────────────────────────────────

    #[test]
    fn decode_unsigned_1byte_value_5() {
        // [0x21, 0x05] → Unsigned(5): tag=2, app, LVT=1, value=0x05
        let (v, n) = decode_application_tag(&[0x21, 0x05]).unwrap();
        assert_eq!(v, BacnetValue::Unsigned(5));
        assert_eq!(n, 2);
    }

    #[test]
    fn decode_unsigned_1byte() {
        let (v, n) = decode_application_tag(&[0x21, 0x42]).unwrap();
        assert_eq!(v, BacnetValue::Unsigned(0x42));
        assert_eq!(n, 2);
    }

    #[test]
    fn decode_unsigned_2byte() {
        let (v, n) = decode_application_tag(&[0x22, 0x01, 0x00]).unwrap();
        assert_eq!(v, BacnetValue::Unsigned(256));
        assert_eq!(n, 3);
    }

    // ── Signed ──────────────────────────────────────────────────────────────

    #[test]
    fn decode_signed_negative_1() {
        // [0x31, 0xFF]: tag=3, app, LVT=1, byte=0xFF → -1i8 → -1i32
        let (v, n) = decode_application_tag(&[0x31, 0xFF]).unwrap();
        assert_eq!(v, BacnetValue::Signed(-1));
        assert_eq!(n, 2);
    }

    #[test]
    fn decode_signed_positive() {
        let (v, n) = decode_application_tag(&[0x31, 0x7F]).unwrap();
        assert_eq!(v, BacnetValue::Signed(127));
        assert_eq!(n, 2);
    }

    // ── Real ────────────────────────────────────────────────────────────────

    #[test]
    fn decode_real_50_0() {
        // 0x42480000 is the IEEE 754 representation of 50.0f32
        let data = [0x44u8, 0x42, 0x48, 0x00, 0x00];
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(v, BacnetValue::Real(50.0f32));
        assert_eq!(n, 5);
    }

    #[test]
    fn decode_real_72_5() {
        let bytes = 72.5f32.to_be_bytes();
        let mut data = vec![0x44u8]; // tag=4, app, LVT=4
        data.extend_from_slice(&bytes);
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(v, BacnetValue::Real(72.5));
        assert_eq!(n, 5);
    }

    #[test]
    fn decode_truncated_real_returns_error() {
        // Real needs 4 value bytes but we give only 2.
        let data = [0x44, 0x00, 0x00];
        assert!(decode_application_tag(&data).is_err());
    }

    // ── Double ──────────────────────────────────────────────────────────────

    #[test]
    fn decode_double() {
        let bytes = 100.0f64.to_be_bytes();
        let mut data = vec![0x55u8]; // tag=5, app, LVT=8 (don't-care)
        data.extend_from_slice(&bytes);
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(v, BacnetValue::Double(100.0));
        assert_eq!(n, 9);
    }

    // ── Enumerated ──────────────────────────────────────────────────────────

    #[test]
    fn decode_enumerated() {
        // [0x91, 0x03]: tag=9, app, LVT=1, value=3
        let (v, n) = decode_application_tag(&[0x91, 0x03]).unwrap();
        assert_eq!(v, BacnetValue::Enumerated(3));
        assert_eq!(n, 2);
    }

    // ── CharacterString ─────────────────────────────────────────────────────

    #[test]
    fn decode_charstring_utf8() {
        // tag=7, LVT=4: encoding byte (0x00=UTF-8) + "Hi!" (3 bytes)
        let data = [0x74, 0x00, b'H', b'i', b'!'];
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(v, BacnetValue::CharacterString("Hi!".into()));
        assert_eq!(n, 5);
    }

    // ── OctetString ─────────────────────────────────────────────────────────

    #[test]
    fn decode_octet_string() {
        let data = [0x62, 0xDE, 0xAD]; // tag=6, app, LVT=2
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(v, BacnetValue::OctetString(vec![0xDE, 0xAD]));
        assert_eq!(n, 3);
    }

    // ── ObjectId ────────────────────────────────────────────────────────────

    #[test]
    fn decode_object_id_device() {
        // Device instance 1 → (8 << 22) | 1 = 0x02000001
        let data = [0xC4, 0x02, 0x00, 0x00, 0x01];
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(
            v,
            BacnetValue::ObjectId {
                object_type: 8,
                instance: 1
            }
        );
        assert_eq!(n, 5);
    }

    #[test]
    fn decode_object_id_analog_input_5() {
        // AnalogInput (type 0) instance 5 → 0x00000005
        let data = [0xC4, 0x00, 0x00, 0x00, 0x05];
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(
            v,
            BacnetValue::ObjectId {
                object_type: 0,
                instance: 5
            }
        );
        assert_eq!(n, 5);
    }

    // ── Context tag skipping ────────────────────────────────────────────────

    #[test]
    fn decode_context_tag_skipped() {
        // Context tag: class=1 (bit3=1), tag_num=0 (bits7-4=0), LVT=4 → 0x0C
        let data = [0x0C, 0x02, 0x00, 0x00, 0x01];
        let (v, n) = decode_application_tag(&data).unwrap();
        assert_eq!(v, BacnetValue::Unknown);
        assert_eq!(n, 5);
    }

    // ── Error cases ─────────────────────────────────────────────────────────

    #[test]
    fn decode_empty_returns_error() {
        assert!(decode_application_tag(&[]).is_err());
    }

    // ── to_f64 ──────────────────────────────────────────────────────────────

    #[test]
    fn to_f64_real() {
        assert_eq!(BacnetValue::Real(72.5f32).to_f64(), Some(72.5f32 as f64));
    }

    #[test]
    fn to_f64_double() {
        assert_eq!(BacnetValue::Double(3.14).to_f64(), Some(3.14));
    }

    #[test]
    fn to_f64_unsigned() {
        assert_eq!(BacnetValue::Unsigned(42).to_f64(), Some(42.0));
    }

    #[test]
    fn to_f64_signed() {
        assert_eq!(BacnetValue::Signed(-5).to_f64(), Some(-5.0));
    }

    #[test]
    fn to_f64_boolean_true() {
        assert_eq!(BacnetValue::Boolean(true).to_f64(), Some(1.0));
    }

    #[test]
    fn to_f64_boolean_false() {
        assert_eq!(BacnetValue::Boolean(false).to_f64(), Some(0.0));
    }

    #[test]
    fn to_f64_enumerated() {
        assert_eq!(BacnetValue::Enumerated(7).to_f64(), Some(7.0));
    }

    #[test]
    fn to_f64_null_returns_none() {
        assert_eq!(BacnetValue::Null.to_f64(), None);
    }

    #[test]
    fn to_f64_unknown_returns_none() {
        assert_eq!(BacnetValue::Unknown.to_f64(), None);
    }

    #[test]
    fn to_f64_char_string_returns_none() {
        assert_eq!(BacnetValue::CharacterString("hello".into()).to_f64(), None);
    }

    #[test]
    fn to_f64_object_id_returns_none() {
        assert_eq!(
            BacnetValue::ObjectId {
                object_type: 0,
                instance: 1
            }
            .to_f64(),
            None
        );
    }

    // ── encode_unsigned ─────────────────────────────────────────────────────

    #[test]
    fn encode_unsigned_1_byte() {
        assert_eq!(encode_unsigned(0), vec![0x21, 0x00]);
        assert_eq!(encode_unsigned(5), vec![0x21, 0x05]);
        assert_eq!(encode_unsigned(255), vec![0x21, 0xFF]);
    }

    #[test]
    fn encode_unsigned_2_bytes() {
        assert_eq!(encode_unsigned(256), vec![0x22, 0x01, 0x00]);
        assert_eq!(encode_unsigned(0xFFFF), vec![0x22, 0xFF, 0xFF]);
    }

    #[test]
    fn encode_unsigned_4_bytes() {
        assert_eq!(
            encode_unsigned(0x01000000),
            vec![0x24, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn encode_unsigned_round_trips() {
        for n in [
            0u32, 1, 127, 128, 255, 256, 0xFFFF, 0x10000, 0xFFFFFF, 0xFFFFFFFF,
        ] {
            let encoded = encode_unsigned(n);
            // Verify the tag byte has tag=2, class=app.
            let tag_num = (encoded[0] >> 4) & 0x0F;
            assert_eq!(tag_num, 2, "tag number should be 2 for Unsigned");
            let class = (encoded[0] >> 3) & 0x01;
            assert_eq!(class, 0, "class should be 0 (application)");
            // Decode and verify round-trip.
            let (v, _) = decode_application_tag(&encoded).unwrap();
            assert_eq!(v, BacnetValue::Unsigned(n), "round-trip failed for {n}");
        }
    }

    // ── encode_object_id_context ────────────────────────────────────────────

    #[test]
    fn encode_object_id_context_tag0() {
        // Context tag 0, AnalogInput (type 0), instance 5.
        // Expected tag byte: (0 << 4) | 0x08 | 0x04 = 0x0C
        // Raw value: (0 << 22) | 5 = 0x00000005
        let encoded = encode_object_id_context(0, 0, 5);
        assert_eq!(encoded, vec![0x0C, 0x00, 0x00, 0x00, 0x05]);
    }

    #[test]
    fn encode_object_id_context_device() {
        // Context tag 3, Device (type 8), instance 1.
        // Tag byte: (3 << 4) | 0x08 | 0x04 = 0x3C
        // Raw: (8 << 22) | 1 = 0x02000001
        let encoded = encode_object_id_context(3, 8, 1);
        assert_eq!(encoded, vec![0x3C, 0x02, 0x00, 0x00, 0x01]);
        assert_eq!(encoded.len(), 5);
    }

    #[test]
    fn encode_object_id_context_sets_context_class_bit() {
        let encoded = encode_object_id_context(1, 2, 10);
        // Bit 3 of the tag byte must be 1 (context class).
        assert_ne!(encoded[0] & 0x08, 0, "context class bit must be set");
    }

    // ── encode_property_id_context ──────────────────────────────────────────

    #[test]
    fn encode_property_id_context_present_value() {
        // Context tag 2, PresentValue = 85 (0x55) → 1-byte encoding.
        // Tag byte: (2 << 4) | 0x08 | 0x01 = 0x29
        let encoded = encode_property_id_context(2, 85);
        assert_eq!(encoded, vec![0x29, 0x55]);
    }

    #[test]
    fn encode_property_id_context_sets_context_class_bit() {
        let encoded = encode_property_id_context(0, 85);
        assert_ne!(encoded[0] & 0x08, 0, "context class bit must be set");
    }

    #[test]
    fn encode_property_id_context_large_value() {
        // Property IDs > 255 must use 2+ bytes.
        let encoded = encode_property_id_context(1, 0x0100);
        // LVT should be 2 (2 value bytes).
        let lvt = (encoded[0] & 0x07) as usize;
        assert_eq!(lvt, 2);
        assert_eq!(encoded.len(), 3);
    }

    // ── Phase B2 edge-case tests ─────────────────────────────────────────────

    /// Two-byte Unsigned: 1476 = 0x05C4, tag 2, LVT=2.
    #[test]
    fn decode_unsigned_two_bytes_1476() {
        // [0x22, 0x05, 0xC4]: tag=2, app, LVT=2, value=0x05C4=1476
        let data = [0x22u8, 0x05, 0xC4];
        let (val, consumed) = decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 3);
        assert!(
            matches!(val, BacnetValue::Unsigned(1476)),
            "expected Unsigned(1476), got {:?}",
            val
        );
    }

    /// Enumerated one byte: tag 9, LVT=1, value=3.
    #[test]
    fn decode_enumerated_segmentation_none() {
        // [0x91, 0x03]: tag=9, app, LVT=1, value=3 (segmentation=none)
        let data = [0x91u8, 0x03];
        let (val, consumed) = decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 2);
        assert!(
            matches!(val, BacnetValue::Enumerated(3)),
            "expected Enumerated(3), got {:?}",
            val
        );
    }

    /// CharString "hello": tag 7, LVT=5 (extended 1-byte length), length=6
    /// (encoding byte 0x00 + 5 chars).
    #[test]
    fn decode_charstring_hello() {
        // tag byte 0x75 = (7 << 4) | 5: tag 7, app, LVT=5 (extended 1-byte length)
        // next byte: 0x06 = length (encoding byte + 5 chars = 6)
        // payload: 0x00 (UTF-8 encoding byte) + "hello" (5 bytes)
        let data = [0x75u8, 0x06, 0x00, b'h', b'e', b'l', b'l', b'o'];
        let (val, consumed) = decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 8, "1 tag byte + 1 length byte + 6 payload bytes");
        match val {
            BacnetValue::CharacterString(s) => assert_eq!(s, "hello"),
            other => panic!("expected CharacterString, got {:?}", other),
        }
    }

    /// CharString "hello" using direct LVT encoding (LVT = byte count ≤ 4).
    /// LVT=3 would encode "hi" (2 chars + encoding byte = 3 bytes total).
    #[test]
    fn decode_charstring_short_direct_lvt() {
        // tag byte 0x73 = (7 << 4) | 3: tag 7, app, LVT=3 (direct count = 3 bytes)
        // payload: 0x00 (UTF-8 encoding byte) + "hi" (2 bytes)
        let data = [0x73u8, 0x00, b'h', b'i'];
        let (val, consumed) = decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 4, "1 tag byte + 3 payload bytes");
        match val {
            BacnetValue::CharacterString(s) => assert_eq!(s, "hi"),
            other => panic!("expected CharacterString, got {:?}", other),
        }
    }

    /// ObjectId: AnalogInput (type 0), instance 1.
    #[test]
    fn decode_object_id_analog_input_instance_1() {
        // tag=12, LVT=4: (0 << 22) | 1 = 0x00000001
        let data = [0xC4u8, 0x00, 0x00, 0x00, 0x01];
        let (val, consumed) = decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 5);
        match val {
            BacnetValue::ObjectId {
                object_type,
                instance,
            } => {
                assert_eq!(object_type, 0, "AnalogInput type = 0");
                assert_eq!(instance, 1);
            }
            other => panic!("expected ObjectId, got {:?}", other),
        }
    }

    /// ObjectId: Device type (8) at max instance 4,194,302.
    #[test]
    fn decode_object_id_device_max_instance() {
        let max_instance = 4_194_302u32;
        let raw: u32 = (8u32 << 22) | max_instance;
        let b = raw.to_be_bytes();
        let data = [0xC4u8, b[0], b[1], b[2], b[3]];
        let (val, consumed) = decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 5);
        match val {
            BacnetValue::ObjectId {
                object_type,
                instance,
            } => {
                assert_eq!(object_type, 8, "Device type = 8");
                assert_eq!(instance, max_instance);
            }
            other => panic!("expected ObjectId, got {:?}", other),
        }
    }

    /// to_f64 for Real with a specific known value.
    #[test]
    fn to_f64_real_72_5() {
        let v = BacnetValue::Real(72.5f32);
        let f = v.to_f64().unwrap();
        assert!((f - 72.5f64).abs() < 0.001, "expected ~72.5, got {f}");
    }

    /// to_f64 for Enumerated returns the numeric value as f64.
    #[test]
    fn to_f64_enumerated_value() {
        assert_eq!(BacnetValue::Enumerated(5).to_f64(), Some(5.0));
        assert_eq!(BacnetValue::Enumerated(0).to_f64(), Some(0.0));
    }

    /// to_f64 for OctetString returns None (non-numeric).
    #[test]
    fn to_f64_octet_string_returns_none() {
        assert_eq!(BacnetValue::OctetString(vec![0xDE, 0xAD]).to_f64(), None);
    }

    /// to_f64 for ObjectId returns None (not a scalar).
    #[test]
    fn to_f64_object_id_none() {
        let v = BacnetValue::ObjectId {
            object_type: 0,
            instance: 1,
        };
        assert!(v.to_f64().is_none());
    }

    /// Signed negative value round-trip: -128 as i8.
    #[test]
    fn decode_signed_min_i8() {
        // [0x31, 0x80]: tag=3, app, LVT=1, byte=0x80 → -128i8 → -128i32
        let (val, consumed) = decode_application_tag(&[0x31u8, 0x80]).unwrap();
        assert_eq!(consumed, 2);
        assert!(
            matches!(val, BacnetValue::Signed(-128)),
            "expected Signed(-128), got {:?}",
            val
        );
    }

    /// Double round-trip for a specific value.
    #[test]
    fn decode_double_neg_inf() {
        let bytes = f64::NEG_INFINITY.to_be_bytes();
        let mut data = vec![0x55u8]; // tag=5, LVT=don't-care
        data.extend_from_slice(&bytes);
        let (val, consumed) = decode_application_tag(&data).unwrap();
        assert_eq!(consumed, 9);
        match val {
            BacnetValue::Double(v) => assert!(v.is_infinite() && v.is_sign_negative()),
            other => panic!("expected Double, got {:?}", other),
        }
    }

    /// Unsigned zero encodes and decodes cleanly.
    #[test]
    fn decode_unsigned_zero() {
        let (val, consumed) = decode_application_tag(&[0x21u8, 0x00]).unwrap();
        assert_eq!(consumed, 2);
        assert!(matches!(val, BacnetValue::Unsigned(0)));
    }

    /// encode_unsigned for value 1476 produces exactly [0x22, 0x05, 0xC4].
    #[test]
    fn encode_unsigned_1476() {
        let encoded = encode_unsigned(1476);
        assert_eq!(encoded, vec![0x22, 0x05, 0xC4]);
        let (val, _) = decode_application_tag(&encoded).unwrap();
        assert!(matches!(val, BacnetValue::Unsigned(1476)));
    }

    // ── Array variant tests ──────────────────────────────────────────────────

    #[test]
    fn array_variant_exists_and_holds_values() {
        let arr = BacnetValue::Array(vec![
            BacnetValue::Real(1.0),
            BacnetValue::Real(2.0),
            BacnetValue::Real(3.0),
        ]);
        match &arr {
            BacnetValue::Array(items) => assert_eq!(items.len(), 3),
            _ => panic!("expected Array variant"),
        }
    }

    #[test]
    fn array_to_f64_returns_first_numeric() {
        let arr = BacnetValue::Array(vec![BacnetValue::Real(42.5), BacnetValue::Unsigned(100)]);
        assert_eq!(arr.to_f64(), Some(42.5_f64));
    }

    #[test]
    fn array_to_f64_empty_returns_none() {
        let arr = BacnetValue::Array(vec![]);
        assert_eq!(arr.to_f64(), None);
    }

    #[test]
    fn array_to_f64_first_non_numeric_returns_none() {
        let arr = BacnetValue::Array(vec![
            BacnetValue::CharacterString("hello".into()),
            BacnetValue::Real(1.0),
        ]);
        assert_eq!(
            arr.to_f64(),
            None,
            "first element is non-numeric so should return None"
        );
    }

    #[test]
    fn array_to_f64_object_ids_returns_none() {
        let arr = BacnetValue::Array(vec![
            BacnetValue::ObjectId {
                object_type: 0,
                instance: 1,
            },
            BacnetValue::ObjectId {
                object_type: 3,
                instance: 0,
            },
        ]);
        assert_eq!(arr.to_f64(), None, "ObjectId is non-numeric");
    }

    #[test]
    fn array_can_hold_mixed_types() {
        let arr = BacnetValue::Array(vec![
            BacnetValue::ObjectId {
                object_type: 0,
                instance: 1,
            },
            BacnetValue::ObjectId {
                object_type: 8,
                instance: 99,
            },
            BacnetValue::ObjectId {
                object_type: 3,
                instance: 0,
            },
        ]);
        if let BacnetValue::Array(items) = &arr {
            let object_ids: Vec<(u16, u32)> = items
                .iter()
                .filter_map(|v| match v {
                    BacnetValue::ObjectId {
                        object_type,
                        instance,
                    } => Some((*object_type, *instance)),
                    _ => None,
                })
                .collect();
            assert_eq!(object_ids, vec![(0, 1), (8, 99), (3, 0)]);
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn array_equality() {
        let a = BacnetValue::Array(vec![BacnetValue::Real(1.0), BacnetValue::Unsigned(2)]);
        let b = BacnetValue::Array(vec![BacnetValue::Real(1.0), BacnetValue::Unsigned(2)]);
        let c = BacnetValue::Array(vec![BacnetValue::Real(1.0)]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn array_to_f64_nested_array_first_element() {
        let inner = BacnetValue::Array(vec![BacnetValue::Real(3.14)]);
        let outer = BacnetValue::Array(vec![inner]);
        // to_f64 on outer → tries first element (inner Array) → tries inner's first (Real(3.14))
        // Real is f32, so we compare via f32→f64 widening to avoid precision mismatch.
        assert_eq!(outer.to_f64(), Some(3.14_f32 as f64));
    }

    #[test]
    fn array_clone_and_debug() {
        let arr = BacnetValue::Array(vec![BacnetValue::Boolean(true)]);
        let cloned = arr.clone();
        assert_eq!(arr, cloned);
        // Debug formatting should not panic
        let _ = format!("{arr:?}");
    }

    #[test]
    fn empty_array_equality() {
        let a = BacnetValue::Array(vec![]);
        let b = BacnetValue::Array(vec![]);
        assert_eq!(a, b);
    }
}

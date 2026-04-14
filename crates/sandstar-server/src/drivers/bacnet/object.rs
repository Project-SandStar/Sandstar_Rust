//! BACnet object types, property identifiers, and driver configuration.
//!
//! Defines the pure data types used by the BACnet/IP driver to describe the
//! objects it polls. No I/O or network operations are performed here.

// ── ObjectType ─────────────────────────────────────────────────────────────

/// BACnet object type numbers per ASHRAE 135-2020 Table 23-2.
///
/// The `#[repr(u16)]` discriminants match the BACnet wire encoding. The
/// [`ObjectType::from_u16`] constructor can be used to decode received values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ObjectType {
    /// Analog Input — tag 0 in the BACnet standard.
    AnalogInput = 0,
    /// Analog Output — tag 1 in the BACnet standard.
    AnalogOutput = 1,
    /// Analog Value — tag 2 in the BACnet standard.
    AnalogValue = 2,
    /// Binary Input — tag 3 in the BACnet standard.
    BinaryInput = 3,
    /// Binary Output — tag 4 in the BACnet standard.
    BinaryOutput = 4,
    /// Binary Value — tag 5 in the BACnet standard.
    BinaryValue = 5,
    /// Device — tag 8 in the BACnet standard.
    Device = 8,
    /// Trend Log — tag 20 in the BACnet standard (reserved for future use).
    TrendLog = 20,
}

impl ObjectType {
    /// Convert a raw `u16` wire value to an [`ObjectType`], returning `None`
    /// for values that do not correspond to a known variant.
    pub fn from_u16(n: u16) -> Option<Self> {
        match n {
            0 => Some(Self::AnalogInput),
            1 => Some(Self::AnalogOutput),
            2 => Some(Self::AnalogValue),
            3 => Some(Self::BinaryInput),
            4 => Some(Self::BinaryOutput),
            5 => Some(Self::BinaryValue),
            8 => Some(Self::Device),
            20 => Some(Self::TrendLog),
            _ => None,
        }
    }

    /// Return `true` if this object type carries an analog (floating-point)
    /// present value.
    ///
    /// Covers [`AnalogInput`](Self::AnalogInput),
    /// [`AnalogOutput`](Self::AnalogOutput), and
    /// [`AnalogValue`](Self::AnalogValue).
    pub fn is_analog(&self) -> bool {
        matches!(
            self,
            Self::AnalogInput | Self::AnalogOutput | Self::AnalogValue
        )
    }

    /// Return `true` if this object type carries a binary (on/off) present value.
    ///
    /// Covers [`BinaryInput`](Self::BinaryInput),
    /// [`BinaryOutput`](Self::BinaryOutput), and
    /// [`BinaryValue`](Self::BinaryValue).
    pub fn is_binary(&self) -> bool {
        matches!(
            self,
            Self::BinaryInput | Self::BinaryOutput | Self::BinaryValue
        )
    }
}

// ── PropertyId ─────────────────────────────────────────────────────────────

/// BACnet property identifiers per ASHRAE 135-2020 Table 23-4.
///
/// The `#[repr(u32)]` discriminants match the BACnet wire encoding used in
/// `ReadProperty-Request` and `ReadProperty-ACK` APDUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PropertyId {
    /// Property 28 — Description: a text string describing the object.
    Description = 28,
    /// Property 76 — Object List: the list of object identifiers on a device.
    ObjectList = 76,
    /// Property 77 — Object Name: a unique text identifier for the object.
    ObjectName = 77,
    /// Property 85 — Present Value: the current value of the object.
    PresentValue = 85,
    /// Property 112 — System Status: the operational status of a device.
    SystemStatus = 112,
    /// Property 117 — Units: the engineering unit of the present value.
    Units = 117,
}

impl PropertyId {
    /// Convert a raw `u32` wire value to a [`PropertyId`], returning `None`
    /// for values that do not correspond to a known variant.
    pub fn from_u32(n: u32) -> Option<Self> {
        match n {
            28 => Some(Self::Description),
            76 => Some(Self::ObjectList),
            77 => Some(Self::ObjectName),
            85 => Some(Self::PresentValue),
            112 => Some(Self::SystemStatus),
            117 => Some(Self::Units),
            _ => None,
        }
    }
}

// ── BacnetObject ───────────────────────────────────────────────────────────

/// A single BACnet point as configured in the driver.
///
/// Each `BacnetObject` maps a Sandstar point to a BACnet object on a remote
/// device. The raw floating-point value returned by the device is transformed
/// as follows before being stored in the engine:
///
/// ```text
/// engineering_value = raw * scale + offset
/// ```
#[derive(Debug, Clone)]
pub struct BacnetObject {
    /// Device instance number that owns this object.
    pub device_id: u32,
    /// BACnet object type stored as a raw `u16` so that unknown / vendor-
    /// specific types can be represented without being rejected.
    pub object_type: u16,
    /// BACnet object instance number within the device.
    pub instance: u32,
    /// Multiplicative scale factor applied to the raw value. Default: `1.0`.
    pub scale: f64,
    /// Additive offset applied after `scale`. Default: `0.0`.
    pub offset: f64,
    /// Optional engineering-unit string (e.g. `"degF"`, `"psi"`, `"%RH"`).
    pub unit: Option<String>,
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ObjectType discriminants match ASHRAE 135 Table 23-2 ───────────────

    #[test]
    fn object_type_analog_input_discriminant() {
        assert_eq!(ObjectType::AnalogInput as u16, 0);
    }

    #[test]
    fn object_type_analog_output_discriminant() {
        assert_eq!(ObjectType::AnalogOutput as u16, 1);
    }

    #[test]
    fn object_type_analog_value_discriminant() {
        assert_eq!(ObjectType::AnalogValue as u16, 2);
    }

    #[test]
    fn object_type_binary_input_discriminant() {
        assert_eq!(ObjectType::BinaryInput as u16, 3);
    }

    #[test]
    fn object_type_binary_output_discriminant() {
        assert_eq!(ObjectType::BinaryOutput as u16, 4);
    }

    #[test]
    fn object_type_binary_value_discriminant() {
        assert_eq!(ObjectType::BinaryValue as u16, 5);
    }

    #[test]
    fn object_type_device_discriminant() {
        assert_eq!(ObjectType::Device as u16, 8);
    }

    #[test]
    fn object_type_trend_log_discriminant() {
        assert_eq!(ObjectType::TrendLog as u16, 20);
    }

    // ── from_u16 round-trips for all variants ──────────────────────────────

    #[test]
    fn from_u16_round_trips_all_variants() {
        let variants = [
            ObjectType::AnalogInput,
            ObjectType::AnalogOutput,
            ObjectType::AnalogValue,
            ObjectType::BinaryInput,
            ObjectType::BinaryOutput,
            ObjectType::BinaryValue,
            ObjectType::Device,
            ObjectType::TrendLog,
        ];
        for v in variants {
            let n = v as u16;
            assert_eq!(
                ObjectType::from_u16(n),
                Some(v),
                "from_u16({n}) should return {v:?}"
            );
        }
    }

    #[test]
    fn from_u16_returns_none_for_unknown_values() {
        assert!(ObjectType::from_u16(99).is_none());
        assert!(ObjectType::from_u16(6).is_none());
        assert!(ObjectType::from_u16(7).is_none());
        assert!(ObjectType::from_u16(9).is_none());
        assert!(ObjectType::from_u16(65535).is_none());
    }

    // ── is_analog correctness ──────────────────────────────────────────────

    #[test]
    fn is_analog_true_for_analog_types() {
        assert!(ObjectType::AnalogInput.is_analog());
        assert!(ObjectType::AnalogOutput.is_analog());
        assert!(ObjectType::AnalogValue.is_analog());
    }

    #[test]
    fn is_analog_false_for_non_analog_types() {
        assert!(!ObjectType::BinaryInput.is_analog());
        assert!(!ObjectType::BinaryOutput.is_analog());
        assert!(!ObjectType::BinaryValue.is_analog());
        assert!(!ObjectType::Device.is_analog());
        assert!(!ObjectType::TrendLog.is_analog());
    }

    // ── is_binary correctness ──────────────────────────────────────────────

    #[test]
    fn is_binary_true_for_binary_types() {
        assert!(ObjectType::BinaryInput.is_binary());
        assert!(ObjectType::BinaryOutput.is_binary());
        assert!(ObjectType::BinaryValue.is_binary());
    }

    #[test]
    fn is_binary_false_for_non_binary_types() {
        assert!(!ObjectType::AnalogInput.is_binary());
        assert!(!ObjectType::AnalogOutput.is_binary());
        assert!(!ObjectType::AnalogValue.is_binary());
        assert!(!ObjectType::Device.is_binary());
        assert!(!ObjectType::TrendLog.is_binary());
    }

    // ── No type is both analog and binary ─────────────────────────────────

    #[test]
    fn no_type_is_both_analog_and_binary() {
        let variants = [
            ObjectType::AnalogInput,
            ObjectType::AnalogOutput,
            ObjectType::AnalogValue,
            ObjectType::BinaryInput,
            ObjectType::BinaryOutput,
            ObjectType::BinaryValue,
            ObjectType::Device,
            ObjectType::TrendLog,
        ];
        for v in variants {
            assert!(
                !(v.is_analog() && v.is_binary()),
                "{v:?} must not be both analog and binary"
            );
        }
    }

    // ── PropertyId discriminants match ASHRAE 135 Table 23-4 ──────────────

    #[test]
    fn property_id_discriminants() {
        assert_eq!(PropertyId::Description as u32, 28);
        assert_eq!(PropertyId::ObjectList as u32, 76);
        assert_eq!(PropertyId::ObjectName as u32, 77);
        assert_eq!(PropertyId::PresentValue as u32, 85);
        assert_eq!(PropertyId::SystemStatus as u32, 112);
        assert_eq!(PropertyId::Units as u32, 117);
    }

    // ── PropertyId::from_u32 round-trips ──────────────────────────────────

    #[test]
    fn property_id_from_u32_round_trips() {
        let variants = [
            PropertyId::Description,
            PropertyId::ObjectList,
            PropertyId::ObjectName,
            PropertyId::PresentValue,
            PropertyId::SystemStatus,
            PropertyId::Units,
        ];
        for p in variants {
            let n = p as u32;
            assert_eq!(
                PropertyId::from_u32(n),
                Some(p),
                "from_u32({n}) should return {p:?}"
            );
        }
    }

    #[test]
    fn property_id_from_u32_returns_none_for_unknown() {
        assert!(PropertyId::from_u32(0).is_none());
        assert!(PropertyId::from_u32(99).is_none());
        assert!(PropertyId::from_u32(u32::MAX).is_none());
    }

    // ── BacnetObject field accessibility ──────────────────────────────────

    #[test]
    fn bacnet_object_fields_accessible() {
        let obj = BacnetObject {
            device_id: 100,
            object_type: ObjectType::AnalogInput as u16,
            instance: 7,
            scale: 2.5,
            offset: -10.0,
            unit: Some("degF".into()),
        };

        assert_eq!(obj.device_id, 100);
        assert_eq!(obj.object_type, 0u16); // AnalogInput = 0
        assert_eq!(obj.instance, 7);
        assert_eq!(obj.scale, 2.5);
        assert_eq!(obj.offset, -10.0);
        assert_eq!(obj.unit.as_deref(), Some("degF"));
    }

    #[test]
    fn bacnet_object_default_scale_and_offset() {
        let obj = BacnetObject {
            device_id: 42,
            object_type: ObjectType::BinaryInput as u16,
            instance: 1,
            scale: 1.0,
            offset: 0.0,
            unit: None,
        };
        assert_eq!(obj.scale, 1.0);
        assert_eq!(obj.offset, 0.0);
        assert!(obj.unit.is_none());
    }

    #[test]
    fn bacnet_object_unknown_type_stored_as_raw_u16() {
        // Unknown vendor-specific type 200 should be storable without panic.
        let obj = BacnetObject {
            device_id: 1,
            object_type: 200,
            instance: 0,
            scale: 1.0,
            offset: 0.0,
            unit: None,
        };
        assert_eq!(obj.object_type, 200);
        // from_u16 on this raw value should return None (unknown type).
        assert!(ObjectType::from_u16(200).is_none());
    }

    #[test]
    fn bacnet_object_clone_is_independent() {
        let original = BacnetObject {
            device_id: 5,
            object_type: ObjectType::AnalogValue as u16,
            instance: 3,
            scale: 1.0,
            offset: 0.0,
            unit: Some("psi".into()),
        };
        let mut cloned = original.clone();
        cloned.device_id = 999;
        cloned.unit = Some("bar".into());

        // Original should be unchanged.
        assert_eq!(original.device_id, 5);
        assert_eq!(original.unit.as_deref(), Some("psi"));
    }
}

/// Errors from the pure Rust Sedona VM interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmError {
    /// Invalid or corrupt scode image
    BadImage(String),
    /// Stack overflow (exceeded max depth)
    StackOverflow,
    /// Stack underflow (popped empty stack)
    StackUnderflow,
    /// Invalid opcode byte encountered
    InvalidOpcode(u8),
    /// Program counter out of bounds
    PcOutOfBounds { pc: usize, code_len: usize },
    /// Native method call failed
    NativeError {
        kit: u8,
        method: u16,
        message: String,
    },
    /// Method not found in dispatch table
    MethodNotFound { block: u16 },
    /// Null pointer dereference in VM memory
    NullPointer,
    /// Type mismatch in slot access
    TypeMismatch { expected: u8, got: u8 },
    /// Array index out of bounds
    ArrayOutOfBounds { index: i32, length: i32 },
    /// Assertion failure in test code
    AssertFailure { method: String },
    /// VM was stopped (normal shutdown)
    Stopped,
    /// Timeout — VM exceeded allowed execution time
    Timeout,
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::BadImage(msg) => write!(f, "bad scode image: {msg}"),
            VmError::StackOverflow => write!(f, "stack overflow"),
            VmError::StackUnderflow => write!(f, "stack underflow"),
            VmError::InvalidOpcode(op) => write!(f, "invalid opcode 0x{op:02X}"),
            VmError::PcOutOfBounds { pc, code_len } => {
                write!(f, "PC {pc} out of bounds (code length {code_len})")
            }
            VmError::NativeError {
                kit,
                method,
                message,
            } => write!(f, "native error kit={kit} method={method}: {message}"),
            VmError::MethodNotFound { block } => write!(f, "method not found: block {block}"),
            VmError::NullPointer => write!(f, "null pointer dereference"),
            VmError::TypeMismatch { expected, got } => {
                write!(f, "type mismatch: expected {expected}, got {got}")
            }
            VmError::ArrayOutOfBounds { index, length } => {
                write!(f, "array index {index} out of bounds (length {length})")
            }
            VmError::AssertFailure { method } => write!(f, "assertion failed in {method}"),
            VmError::Stopped => write!(f, "VM stopped"),
            VmError::Timeout => write!(f, "VM execution timeout"),
        }
    }
}

impl std::error::Error for VmError {}

/// Convenience alias for VM operations.
pub type VmResult<T> = Result<T, VmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_bad_image() {
        let e = VmError::BadImage("truncated header".into());
        assert_eq!(e.to_string(), "bad scode image: truncated header");
    }

    #[test]
    fn display_stack_overflow() {
        assert_eq!(VmError::StackOverflow.to_string(), "stack overflow");
    }

    #[test]
    fn display_stack_underflow() {
        assert_eq!(VmError::StackUnderflow.to_string(), "stack underflow");
    }

    #[test]
    fn display_invalid_opcode() {
        let e = VmError::InvalidOpcode(0xFF);
        assert_eq!(e.to_string(), "invalid opcode 0xFF");
    }

    #[test]
    fn display_pc_out_of_bounds() {
        let e = VmError::PcOutOfBounds {
            pc: 1024,
            code_len: 512,
        };
        assert_eq!(e.to_string(), "PC 1024 out of bounds (code length 512)");
    }

    #[test]
    fn display_native_error() {
        let e = VmError::NativeError {
            kit: 4,
            method: 12,
            message: "I2C timeout".into(),
        };
        assert_eq!(e.to_string(), "native error kit=4 method=12: I2C timeout");
    }

    #[test]
    fn display_method_not_found() {
        let e = VmError::MethodNotFound { block: 42 };
        assert_eq!(e.to_string(), "method not found: block 42");
    }

    #[test]
    fn display_null_pointer() {
        assert_eq!(VmError::NullPointer.to_string(), "null pointer dereference");
    }

    #[test]
    fn display_type_mismatch() {
        let e = VmError::TypeMismatch {
            expected: 1,
            got: 3,
        };
        assert_eq!(e.to_string(), "type mismatch: expected 1, got 3");
    }

    #[test]
    fn display_array_out_of_bounds() {
        let e = VmError::ArrayOutOfBounds {
            index: 10,
            length: 5,
        };
        assert_eq!(e.to_string(), "array index 10 out of bounds (length 5)");
    }

    #[test]
    fn display_assert_failure() {
        let e = VmError::AssertFailure {
            method: "testAdd".into(),
        };
        assert_eq!(e.to_string(), "assertion failed in testAdd");
    }

    #[test]
    fn display_stopped() {
        assert_eq!(VmError::Stopped.to_string(), "VM stopped");
    }

    #[test]
    fn display_timeout() {
        assert_eq!(VmError::Timeout.to_string(), "VM execution timeout");
    }

    #[test]
    fn error_trait_is_implemented() {
        let e: Box<dyn std::error::Error> = Box::new(VmError::StackOverflow);
        // Verify we can use it as a trait object
        assert_eq!(e.to_string(), "stack overflow");
    }

    #[test]
    fn vm_result_ok() {
        let r: VmResult<i32> = Ok(42);
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn vm_result_err() {
        let r: VmResult<i32> = Err(VmError::NullPointer);
        assert!(r.is_err());
        assert_eq!(r.unwrap_err(), VmError::NullPointer);
    }

    #[test]
    fn clone_and_eq() {
        let e1 = VmError::InvalidOpcode(0xAB);
        let e2 = e1.clone();
        assert_eq!(e1, e2);
    }
}

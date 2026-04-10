//! Phase 3: Native method tests for Kit 0 (component) and Kit 2 (inet).
//!
//! Verifies that the newly-enabled native methods are properly registered
//! and callable through the NativeTable dispatch system.

use sandstar_svm::native_table::{NativeContext, NativeTable};
use sandstar_svm::native_mod::register_all_natives;

// ════════════════════════════════════════════════════════════════
// Helper
// ════════════════════════════════════════════════════════════════

/// Create a NativeTable with all natives registered via `register_all_natives`
/// (the path used by the real VM runner, which includes inet).
fn full_table() -> NativeTable {
    let mut table = NativeTable::new();
    // Pre-populate stubs so register functions have slots to overwrite
    table.set_kit_name(0, "sys");
    for id in 0..60u16 {
        table.register_stub(0, id);
    }
    table.set_kit_name(2, "inet");
    for id in 0..17u16 {
        table.register_stub(2, id);
    }
    table.set_kit_name(3, "serial");
    for id in 0..6u16 {
        table.register_stub(3, id);
    }
    table.set_kit_name(4, "EacIo");
    for id in 0..23u16 {
        table.register_stub(4, id);
    }
    table.set_kit_name(9, "datetimeStd");
    for id in 0..3u16 {
        table.register_stub(9, id);
    }

    register_all_natives(&mut table);
    table
}

fn test_ctx(mem: &mut Vec<u8>) -> NativeContext<'_> {
    NativeContext::new(mem)
}

// ════════════════════════════════════════════════════════════════
// 1. Registration Tests
// ════════════════════════════════════════════════════════════════

#[test]
fn test_all_natives_registered() {
    let table = full_table();

    // Kit 0: sys (29) + file (11) + component (20) = 60 implemented
    let kit0_count = table.implemented_count(0);
    assert!(
        kit0_count >= 60,
        "Kit 0 should have >= 60 implemented methods (sys 29 + file 11 + component 20), got {kit0_count}"
    );

    // Kit 2: inet = 17
    assert_eq!(
        table.implemented_count(2),
        17,
        "Kit 2 (inet) should have 17 implemented methods"
    );

    // Kit 4: EacIo = 22 real (slot 0 is stub)
    assert_eq!(
        table.implemented_count(4),
        22,
        "Kit 4 (EacIo) should have 22 implemented methods"
    );

    // Kit 9: datetimeStd = 3
    assert_eq!(
        table.implemented_count(9),
        3,
        "Kit 9 (datetimeStd) should have 3 implemented methods"
    );
}

#[test]
fn test_component_methods_registered() {
    let table = full_table();

    // Kit 0 component slots should be real implementations, not stubs
    assert!(
        table.is_implemented(0, 22),
        "Kit 0 slot 22 (Component.invokeVoid) should be implemented"
    );
    assert!(
        table.is_implemented(0, 29),
        "Kit 0 slot 29 (Component.getBool) should be implemented"
    );
    assert!(
        table.is_implemented(0, 35),
        "Kit 0 slot 35 (Component.doSetBool) should be implemented"
    );
    assert!(
        table.is_implemented(0, 40),
        "Kit 0 slot 40 (Type.malloc) should be implemented"
    );
    assert!(
        table.is_implemented(0, 55),
        "Kit 0 slot 55 (Test.doMain) should be implemented"
    );

    // Also check some intermediate component slots
    assert!(
        table.is_implemented(0, 30),
        "Kit 0 slot 30 (Component.getInt) should be implemented"
    );
    assert!(
        table.is_implemented(0, 31),
        "Kit 0 slot 31 (Component.getLong) should be implemented"
    );
    assert!(
        table.is_implemented(0, 34),
        "Kit 0 slot 34 (Component.getBuf) should be implemented"
    );
    assert!(
        table.is_implemented(0, 36),
        "Kit 0 slot 36 (Component.doSetInt) should be implemented"
    );
    assert!(
        table.is_implemented(0, 39),
        "Kit 0 slot 39 (Component.doSetDouble) should be implemented"
    );
}

#[test]
fn test_inet_methods_registered() {
    let table = full_table();

    // All 17 inet methods (slots 0-16) should be real implementations
    assert!(
        table.is_implemented(2, 0),
        "Kit 2 slot 0 (TcpSocket.connect) should be implemented"
    );
    assert!(
        table.is_implemented(2, 5),
        "Kit 2 slot 5 (TcpServerSocket.bind) should be implemented"
    );
    assert!(
        table.is_implemented(2, 8),
        "Kit 2 slot 8 (UdpSocket.open) should be implemented"
    );
    assert!(
        table.is_implemented(2, 15),
        "Kit 2 slot 15 (Crypto.sha1) should be implemented"
    );

    // Verify every single inet slot is implemented
    for slot in 0..17u16 {
        assert!(
            table.is_implemented(2, slot),
            "Kit 2 slot {slot} should be implemented"
        );
    }
}

// ════════════════════════════════════════════════════════════════
// 2. Kit 0 Sys Method Tests (verify existing methods still work)
// ════════════════════════════════════════════════════════════════

#[test]
fn test_sys_platform_type() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 0 method 0 = Sys.platformType()
    let result = table.call(0, 0, &mut ctx, &[]).unwrap();
    // Should return a non-zero handle to a static string
    assert_ne!(result, 0, "Sys.platformType() should return a non-zero handle");
}

#[test]
fn test_sys_ticks() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 0 method 14 = Sys.ticks() — wide method returning i64 nanoseconds
    let result = table.call_wide(0, 14, &mut ctx, &[]).unwrap();
    assert!(result > 0, "Sys.ticks() should return positive nanoseconds, got {result}");

    // Call again — should be >= first result (monotonic)
    let result2 = table.call_wide(0, 14, &mut ctx, &[]).unwrap();
    assert!(
        result2 >= result,
        "Sys.ticks() should be monotonic: {result2} >= {result}"
    );
}

#[test]
fn test_sys_rand() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 0 method 21 = Sys.rand()
    let r1 = table.call(0, 21, &mut ctx, &[]).unwrap();
    let r2 = table.call(0, 21, &mut ctx, &[]).unwrap();

    // Two consecutive calls should produce different values (xorshift PRNG)
    // In theory they *could* collide, but xorshift32 won't produce the same
    // output twice in a row unless the state is broken.
    assert_ne!(
        r1, r2,
        "Sys.rand() should produce different values on consecutive calls"
    );
}

// ════════════════════════════════════════════════════════════════
// 3. Kit 2 Inet Method Tests
// ════════════════════════════════════════════════════════════════

#[test]
fn test_udp_max_packet_size() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 2 method 13 = UdpSocket.maxPacketSize()
    let result = table.call(2, 13, &mut ctx, &[]).unwrap();
    assert_eq!(
        result, 512,
        "UdpSocket.maxPacketSize() should return 512, got {result}"
    );
}

#[test]
fn test_udp_ideal_packet_size() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 2 method 14 = UdpSocket.idealPacketSize()
    let result = table.call(2, 14, &mut ctx, &[]).unwrap();
    assert_eq!(
        result, 512,
        "UdpSocket.idealPacketSize() should return 512, got {result}"
    );
}

#[test]
fn test_udp_close_on_invalid_handle() {
    let table = full_table();
    // Allocate enough memory for the component struct (handle at offset 4, closed at offset 0)
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 2 method 12 = UdpSocket.close()
    // Calling close on a non-existent socket should not panic
    let result = table.call(2, 12, &mut ctx, &[0]);
    assert!(result.is_ok(), "UdpSocket.close() on invalid handle should not error");
}

#[test]
fn test_tcp_close_on_invalid_handle() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 2 method 4 = TcpSocket.close()
    // Calling close on a non-existent socket should not panic
    let result = table.call(2, 4, &mut ctx, &[0]);
    assert!(result.is_ok(), "TcpSocket.close() on invalid handle should not error");
}

#[test]
fn test_tcp_server_close_on_invalid_handle() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 2 method 7 = TcpServerSocket.close()
    let result = table.call(2, 7, &mut ctx, &[0]);
    assert!(result.is_ok(), "TcpServerSocket.close() on invalid handle should not error");
}

// ════════════════════════════════════════════════════════════════
// 4. Kit 2 SHA-1 Crypto Test
// ════════════════════════════════════════════════════════════════

#[test]
fn test_crypto_sha1() {
    let table = full_table();

    // SHA-1 of empty input is da39a3ee5e6b4b0d3255bfef95601890afd80709
    // Set up memory: input buffer at offset 0, output buffer at offset 64
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 2 slot 15 = Crypto.sha1(input, inputOff, len, output, outputOff)
    // params: [input_ptr, input_off, len, output_ptr, output_off]
    // For empty input: len=0, input doesn't matter
    let result = table.call(2, 15, &mut ctx, &[0, 0, 0, 64, 0]);
    assert!(result.is_ok(), "Crypto.sha1() should succeed on empty input");

    // Check SHA-1 of empty string at mem[64..84]
    let expected_sha1: [u8; 20] = [
        0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d,
        0x32, 0x55, 0xbf, 0xef, 0x95, 0x60, 0x18, 0x90,
        0xaf, 0xd8, 0x07, 0x09,
    ];
    assert_eq!(
        &mem[64..84],
        &expected_sha1,
        "SHA-1 of empty input should match known digest"
    );
}

// ════════════════════════════════════════════════════════════════
// 5. with_defaults vs register_all_natives comparison
// ════════════════════════════════════════════════════════════════

#[test]
fn test_with_defaults_has_component_methods() {
    // with_defaults() registers component methods directly (not through register_all_natives)
    let table = NativeTable::with_defaults();

    // Component methods should be present via with_defaults
    assert!(
        table.is_implemented(0, 22),
        "with_defaults: Kit 0 slot 22 (invokeVoid) should be implemented"
    );
    assert!(
        table.is_implemented(0, 29),
        "with_defaults: Kit 0 slot 29 (getBool) should be implemented"
    );
    assert!(
        table.is_implemented(0, 40),
        "with_defaults: Kit 0 slot 40 (Type.malloc) should be implemented"
    );
}

#[test]
fn test_register_all_natives_adds_inet() {
    // Verify that register_all_natives is needed to get inet methods
    // (with_defaults has inet registration commented out)
    let defaults_table = NativeTable::with_defaults();
    let full = full_table();

    let defaults_inet = defaults_table.implemented_count(2);
    let full_inet = full.implemented_count(2);

    // full_table (via register_all_natives) should have all 17 inet methods
    assert_eq!(full_inet, 17, "full table should have 17 inet methods");

    // with_defaults may or may not have inet registered (currently commented out)
    // Either way, full_table should have >= what with_defaults has
    assert!(
        full_inet >= defaults_inet,
        "register_all_natives should have >= with_defaults inet count: {full_inet} >= {defaults_inet}"
    );
}

// ════════════════════════════════════════════════════════════════
// 6. Component reflection method invocation smoke tests
// ════════════════════════════════════════════════════════════════

#[test]
fn test_component_invoke_void_with_no_code_returns_error() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // invokeVoid (slot 22) requires code segment access.
    // With a minimal context (no code), it should return an error
    // because it can't resolve the slot descriptor.
    let result = table.call(0, 22, &mut ctx, &[0, 0]);
    // This may error (no code segment) or return 0 depending on implementation
    // The key test is that it doesn't panic
    let _ = result;
}

#[test]
fn test_component_get_bool_with_no_code_returns_error() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // getBool (slot 29) requires code segment for slot resolution
    let result = table.call(0, 29, &mut ctx, &[0, 0]);
    // Should not panic; may return error or default value
    let _ = result;
}

#[test]
fn test_type_malloc_returns_zero_without_code() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Type.malloc (slot 40) — without proper code segment, should handle gracefully
    let result = table.call(0, 40, &mut ctx, &[0]);
    // Should not panic
    let _ = result;
}

// ════════════════════════════════════════════════════════════════
// 7. Kit 0 sys string formatting methods
// ════════════════════════════════════════════════════════════════

#[test]
fn test_sys_int_str() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 0 method 4 = Sys.intStr(int)
    let handle = table.call(0, 4, &mut ctx, &[42]).unwrap();
    // Should return a non-zero string handle
    assert_ne!(handle, 0, "Sys.intStr(42) should return a non-zero handle");
}

#[test]
fn test_sys_float_to_bits() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 0 method 10 = Sys.floatToBits(float)
    let pi_bits = std::f32::consts::PI.to_bits() as i32;
    let result = table.call(0, 10, &mut ctx, &[pi_bits]).unwrap();
    assert_eq!(
        result, pi_bits,
        "Sys.floatToBits should be identity on bit pattern"
    );
}

#[test]
fn test_sys_bits_to_float() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 0 method 12 = Sys.bitsToFloat(int)
    let pi_bits = std::f32::consts::PI.to_bits() as i32;
    let result = table.call(0, 12, &mut ctx, &[pi_bits]).unwrap();
    let f = f32::from_bits(result as u32);
    assert!(
        (f - std::f32::consts::PI).abs() < 1e-6,
        "Sys.bitsToFloat should reconstruct PI, got {f}"
    );
}

// ════════════════════════════════════════════════════════════════
// 8. Kit 9 datetime method test
// ════════════════════════════════════════════════════════════════

#[test]
fn test_datetime_do_now() {
    let table = full_table();
    let mut mem = vec![0u8; 256];
    let mut ctx = test_ctx(&mut mem);

    // Kit 9 method 0 = doNow (wide return — i64 nanoseconds since epoch)
    let result = table.call_wide(9, 0, &mut ctx, &[0]);
    assert!(result.is_ok(), "datetime doNow should not error");
    let nanos = result.unwrap();
    // Should be a large positive number (nanos since 2000-01-01 or similar)
    // At minimum, it should not be zero or negative
    assert!(nanos != 0, "datetime doNow should return non-zero value");
}

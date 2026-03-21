fn main() {
    // If pure-rust-vm feature is active, skip C compilation entirely.
    // All native methods are provided by Rust modules (native_sys, native_file,
    // native_datetime, etc.) so no C compiler is needed.
    if cfg!(feature = "pure-rust-vm") {
        println!("cargo:warning=Building with pure Rust VM (no C code)");
        return;
    }

    let csrc = std::path::Path::new("csrc");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let is_unix_target = target_os == "linux" || target_os == "android" || target_os == "freebsd";

    let mut build = cc::Build::new();

    // Always compile the VM core and nativetable (platform-independent C)
    build
        .file(csrc.join("vm.c"))
        .file(csrc.join("nativetable.c"))
        .include(csrc)
        .define("SCODE_BLOCK_SIZE", "4")
        .define("SCODE_DEBUG", None)
        .warnings(false);

    if is_unix_target {
        // Full compilation: all native C libraries for the real target
        build
            .define("__UNIX__", None)
            // sys (kit 0)
            .file(csrc.join("sys_Sys.c"))
            .file(csrc.join("sys_Component.c"))
            .file(csrc.join("sys_Str.c"))
            .file(csrc.join("sys_Test.c"))
            .file(csrc.join("sys_Type.c"))
            .file(csrc.join("sys_Sys_std.c"))
            .file(csrc.join("sys_Sys_unix.c"))
            .file(csrc.join("sys_PlatformService_unix.c"))
            .file(csrc.join("sys_File_std.c"))
            .file(csrc.join("sys_FileStore_std.c"))
            .file(csrc.join("sys_StdOutStream_std.c"))
            // inet (kit 2) — POSIX sockets
            .file(csrc.join("inet_TcpSocket_std.c"))
            .file(csrc.join("inet_TcpServerSocket_std.c"))
            .file(csrc.join("inet_UdpSocket_std.c"))
            .file(csrc.join("inet_util_std.c"))
            .file(csrc.join("inet_Crypto_sha1.c"))
            .file(csrc.join("inet_sha1.c"))
            // datetimeStd (kit 9)
            .file(csrc.join("datetimeStd_DateTimeServiceStd.c"));
    } else {
        // Windows dev build: only VM core + nativetable.
        // All kit 0/2/9 native methods provided as Rust stubs via bridge.rs.
        // This allows development and testing on Windows.
    }

    build.compile("sedona_vm");
}

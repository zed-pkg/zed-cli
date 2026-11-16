//! Keep the shipping Windows CLI's startup stack usable for its full
//! command-dispatch and embedded flags-2-env admission path.
//!
//! MSVC's default executable main-thread stack reservation is 1 MiB. The
//! complete Zed CLI has a large typed command graph; on Windows the real
//! executable overflows before reporting ordinary add-project validation
//! errors, even though the identical CLI tests pass on Linux/macOS and
//! Windows' Rust test-worker threads. Increasing the executable's reserved
//! virtual address space does not eagerly commit 8 MiB of physical memory.
//! Limit the setting to the Windows/MSVC 'zed' executable, not libraries,
//! test harnesses, or the other zed-* helper binaries.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bin=zed=/STACK:8388608");
    }
}

//! Link flags the windows-gnu build needs, applied from inside the package.
//!
//! These used to live only in `.cargo/config.toml`, which works for anyone building the repo and
//! fails for everyone else: cargo does not put that file in a published `.crate`, so
//! `cargo install cram-cli` linked without them and died on the winpthread clash described below.
//! A build script *is* published, so the flags travel with the crate.
//!
//! `.cargo/config.toml` still carries the same flags. Applying them twice is harmless (the linker
//! takes `-static` and `--allow-multiple-definition` idempotently, and a repeated `-ladvapi32` just
//! resolves the same imports), and keeping it means `cargo build` in a fresh clone behaves
//! identically whether or not the build script ran.
//!
//! Why each one, on windows-gnu only:
//!
//!   * The `unrar` crate compiles the UnRAR engine as C++, dragging in libstdc++, libgcc and
//!     winpthread. `-static` folds libstdc++ and libgcc into the exe so no `libstdc++-6.dll` or
//!     `libgcc_s_seh-1.dll` has to sit beside it.
//!   * Rust's `libpthread.a` and the C++ side's pthread both define `pthread_mutex_lock` and
//!     friends, which GNU ld refuses. `--allow-multiple-definition` keeps the first definition. A
//!     side effect is that the *dynamic* winpthread wins, so the exe keeps one
//!     `libwinpthread-1.dll` dependency; that DLL ships beside the binary in releases.
//!   * `-static` reorders the link line and drops the Win32 registry/security/crypto imports the
//!     UnRAR C++ needs, and GNU ld is single-pass, so `-ladvapi32` has to come last.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let gnu = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    if !(windows && gnu) {
        return;
    }

    // `-bins` rather than the blanket form: these apply to the `cram` executable, not to build
    // scripts or test harnesses, and passing them to a build script's own link step is both
    // pointless and a way to break cross-compilation.
    for arg in [
        "-static",
        "-Wl,--allow-multiple-definition",
        // Last, deliberately. See the module comment.
        "-ladvapi32",
    ] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
}

// Only does anything for `fuse` on Windows: tells the MSVC linker to
// delay-load WinFSP's DLL so `src/fuse/windows.rs` can point the loader at
// WinFsp's install directory (read from the registry) before first use,
// instead of requiring the DLL to sit next to ixr.exe. See docs/FUSE.md.
fn main() {
    #[cfg(all(windows, feature = "fuse"))]
    winfsp_wrs_build::build();

    // macOS analog of the above: `fuser`'s build script hard-links
    // libfuse.2.dylib (from macFUSE) via a plain `-lfuse`, so dyld refuses to
    // even start the binary if macFUSE isn't installed — breaking every
    // command, not just mount. Re-emitting the same lib as `-weak-lfuse`
    // after it overrides the load command to weak (ld64 takes the last
    // strength given for a dylib), so dyld tolerates it being missing and
    // only errors if fuse code is actually reached at runtime.
    #[cfg(all(target_os = "macos", feature = "fuse"))]
    println!("cargo::rustc-link-arg-bins=-Wl,-weak-lfuse");
}

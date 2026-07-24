// Only does anything for `fuse` on Windows: tells the MSVC linker to
// delay-load WinFSP's DLL so `src/fuse/windows.rs` can point the loader at
// WinFsp's install directory (read from the registry) before first use,
// instead of requiring the DLL to sit next to ixr.exe. See docs/FUSE.md.
fn main() {
    #[cfg(all(windows, feature = "fuse"))]
    winfsp_wrs_build::build();
}

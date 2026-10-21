fn main() {
    // The AEC links WebRTC, and on a machine with a system abseil it links
    // THOSE shared libraries instead of the bundled static copy. A release
    // tarball therefore has to ship them next to the bridge — and for the
    // loader to look there, the bridge itself needs a runpath: it is
    // dlopen'ed by the Flutter engine, whose own runpath does not apply to
    // a different object's dependencies (verified with LD_DEBUG=libs: the
    // bundle's lib/ is never searched without this).
    //
    // `$ORIGIN` is the directory holding this .so (bundle/lib). Harmless
    // when nothing extra lives there.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    println!("cargo:rerun-if-changed=build.rs");
}

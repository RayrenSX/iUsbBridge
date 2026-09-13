fn main() {
    println!("cargo:rerun-if-changed=src/legacy_usbmux.c");
    cc::Build::new()
        .file("src/legacy_usbmux.c")
        .compile("legacy_usbmux");
}

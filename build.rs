fn main() {
    // Declare the custom cfgs used across the client so rustc does not emit
    // "unexpected cfg condition" warnings for them.
    println!("cargo::rustc-check-cfg=cfg(zisk_hints)");
    println!("cargo::rustc-check-cfg=cfg(zisk_hints_debug)");
    println!("cargo::rustc-check-cfg=cfg(target_vendor, values(\"zisk\"))");
}

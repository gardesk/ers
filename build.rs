fn main() {
    // Link SkyLight private framework for WindowServer access
    println!("cargo:rustc-link-lib=framework=SkyLight");
    // Link CoreGraphics for drawing
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    // Link CoreFoundation for CFRunLoop
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}

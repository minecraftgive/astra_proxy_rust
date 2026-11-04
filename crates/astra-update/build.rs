fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=ASTRA_UPDATE_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}

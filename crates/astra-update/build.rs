fn main() {
    println!(
        "cargo:rustc-env=ASTRA_UPDATE_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}

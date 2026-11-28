fn main() {
    // rust-i18n reads `locales/` at compile time without telling cargo.
    println!("cargo:rerun-if-changed=locales");
}

fn main() {
    // rust-i18n reads `locales/` at compile time, but cargo does not see that: rebuild when
    // the translations change, or edits to them never reach the app.
    println!("cargo:rerun-if-changed=locales");
}

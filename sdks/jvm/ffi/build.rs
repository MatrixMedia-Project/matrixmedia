// Build script: hands the .udl file to uniffi-bindgen so the generated
// scaffolding is available to lib.rs at compile time via `uniffi::include_scaffolding!`.
//
// Reference pattern: https://mozilla.github.io/uniffi-rs/tutorial/Rust_scaffolding.html

fn main() {
    uniffi::generate_scaffolding("./matrix_sdk_ffi.udl")
        .expect("Failed to generate UniFFI scaffolding from matrix_sdk_ffi.udl");
}

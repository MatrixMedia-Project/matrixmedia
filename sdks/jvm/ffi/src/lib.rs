//! MatrixMedia JVM FFI crate.
//!
//! This is the scaffold. It exposes ONE function (`version`) through UniFFI
//! so the build pipeline can be validated before the real matrix-rust-sdk
//! surface is wired in.
//!
//! TODO(rust-dev): expand this module to wrap the relevant matrix-sdk-ffi
//! API once the dependency is enabled in Cargo.toml.

uniffi::include_scaffolding!("matrix_sdk_ffi");

/// Returns the crate version. Smoke test for the FFI pipeline.
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }
}

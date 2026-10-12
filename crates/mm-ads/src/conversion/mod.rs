//! Ad-creative conversion policy (internal spec 2026-10-11): what profile v1 encodes, which
//! uploads are accepted, and how an encoded file is judged. Pure — no processes, no files, no
//! database. mm-media-worker does the I/O and hands this module strings and bytes.
pub mod error;
pub mod probe;

pub use error::{ConversionError, ErrorCode};
pub use probe::{Loudness, Ratio, SourceInfo, SourceKind, VideoInfo};

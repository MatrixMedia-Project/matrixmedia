//! MatrixMedia Discovery & Recommendations Engine
//!
//! Provides trending calculation, signal collection, and personalized
//! discovery feeds for the MatrixMedia streaming service.
//!
//! # Architecture
//!
//! ```text
//! mm-api (thin handlers)
//!    |
//!    v
//! mm-recommendations (business logic)
//!    |--- trending.rs    Rule-based trending engine with moka cache
//!    |--- signals.rs     Signal collection (views, likes, shares)
//!    └--- discovery.rs   Personalized feed assembly (for-you, related)
//! ```

pub mod discovery;
pub mod signals;
pub mod trending;

pub use discovery::DiscoveryService;
pub use signals::{record_like, record_share, record_stream_view};
pub use trending::{TrendingEngine, TrendingStream};

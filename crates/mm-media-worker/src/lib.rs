//! mm-media-worker (internal spec 2026-10-11 §5.2): claims ad conversion jobs and runs ffmpeg on
//! them in a sandboxed container. Policy — what to encode and how to judge it — lives in
//! `mm_ads::conversion`; this crate is process plumbing: files, ffmpeg, the queue, the sweep.
pub mod app;
pub mod env;
pub mod ffmpeg;
pub mod job;
pub mod legacy;
pub mod media;
pub mod store;
pub mod sweeper;

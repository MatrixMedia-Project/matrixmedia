//! LNURL-pay client (LUD-06 + LUD-16 Lightning Address resolution).
//!
//! Lets MatrixMedia request a BOLT11 invoice from a creator's wallet without
//! the operator ever holding funds. The flow is:
//!
//! 1. Creator publishes a Lightning Address `name@domain.tld` (LUD-16).
//! 2. mm-payment resolves it: `GET https://{domain}/.well-known/lnurlp/{name}`.
//!    Response is a `LnurlPayMetadata` document (LUD-06).
//! 3. mm-payment requests an invoice: `GET {callback}?amount={millisats}&comment={c}`.
//!    Response is a `LnurlPayInvoice` containing a fresh BOLT11 invoice.
//! 4. The donor's wallet pays that invoice (NWC, scan, copy/paste, etc.).
//!    Settlement is wallet-to-wallet — operator is never in the path.
//!
//! This is the canonical M1 payment rail per `mm-demo-path-pivot.md`.

pub mod client;
pub mod types;

pub use client::LnurlPayClient;
pub use types::{
    LnurlError, LnurlPayInvoice, LnurlPayMetadata, parse_lightning_address,
};

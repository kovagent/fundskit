//! `fundskit` — SEC Form 13F institutional holdings for Rust.
//!
//! Fetches per-quarter parquet files on demand from GitHub raw, caches them
//! locally with ETag revalidation, and falls back to stale cache on network
//! errors. No API keys. Offline after the first successful fetch of each
//! quarter file.
//!
//! Data comes from the SEC's public-domain Form 13F Data Sets. Each row is one
//! reported holding (one information-table line); dates are `i32` `YYYYMMDD`.
//!
//! Positions are identified by `cusip` plus `issuer_name`. The `ticker` column
//! is present for forward-compatibility but left empty: there is no free, clean
//! CUSIP-to-ticker map, and fabricating one would be wrong. The filing manager
//! is mapped to its CIK and name.
//!
//! # Quick start — free functions
//!
//! ```no_run
//! use fundskit::holders_of;
//!
//! #[tokio::main]
//! async fn main() -> fundskit::Result<()> {
//!     for h in holders_of("037833100").await?.iter().take(5) {
//!         println!("{} {} shares ${}", h.manager_name, h.shares, h.value_usd);
//!     }
//!     Ok(())
//! }
//! ```
//!
//! For connection-pool reuse across many lookups, create a [`Fundskit`] client
//! once and call its methods instead of the free functions.
//!
//! # Environment overrides
//!
//! | Variable | Effect |
//! |---|---|
//! | `FUNDSKIT_BASE_URL` | Replace the GitHub raw origin URL |
//! | `FUNDSKIT_CACHE_DIR` | Override `~/.cache/fundskit/` |
//! | `FUNDSKIT_MIRROR_URL` | Override the jsDelivr CDN mirror |
#![forbid(unsafe_code)]

mod error;
pub use error::{Error, Result};

mod record;
pub use record::Holding;

pub mod parquet_io;
pub use parquet_io::{read_holdings, write_holdings};

mod fetcher;

mod client;
pub use client::{holders_of, holdings_by_manager, latest_period, Fundskit};

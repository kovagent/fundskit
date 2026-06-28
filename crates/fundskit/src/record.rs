//! The 13F holding record.
//!
//! One [`Holding`] is one line of a Form 13F information table: a single
//! security position held by an institutional manager as of a quarter-end.
//! Dates are stored as `i32` in `YYYYMMDD` form (e.g. `20240331`) so
//! comparisons are integer-cheap and need no calendar library on the hot path.
use serde::{Deserialize, Serialize};

/// One reported 13F holding (one row in the bundled parquet).
///
/// `report_period` is the quarter-end the position is reported as of;
/// `filed_date` is when the filing was submitted. `value_usd` is normalized to
/// whole US dollars (the SEC reported value in thousands for filings made
/// before 2023 and in whole dollars from 2023 on; both are stored in dollars
/// here). `ticker` is left empty: there is no free, clean CUSIP-to-ticker map,
/// so positions are identified by `cusip` plus `issuer_name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Holding {
    pub report_period: i32,
    pub filed_date: i32,
    pub accession: String,
    pub manager_cik: u32,
    pub manager_name: String,
    pub issuer_name: String,
    pub cusip: String,
    /// Best-effort ticker; empty unless a reliable CUSIP map is available.
    pub ticker: String,
    pub title_of_class: String,
    /// Market value of the position in whole US dollars.
    pub value_usd: i64,
    /// Share or principal amount, per `share_type`.
    pub shares: i64,
    /// `SH` for shares, `PRN` for principal amount.
    pub share_type: String,
    /// Empty for a long position, else `Put` or `Call`.
    pub put_call: String,
    /// Investment discretion: `SOLE`, `DFND`, `OTR`, or empty.
    pub discretion: String,
}

impl Holding {
    /// `true` for a put or call option position (`put_call` non-empty).
    pub fn is_option(&self) -> bool {
        !self.put_call.is_empty()
    }
}

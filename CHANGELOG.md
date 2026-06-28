<!-- Canonical CHANGELOG header for every *kit. The body keeps each kit's real
release history; only this top block is standardized. -->
# Changelog

All notable changes to fundskit are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0]

Initial release.

- Async `Fundskit` client plus blocking siblings and one-shot free functions.
- Query surface: `holders_of`, `holdings_by_manager`, `latest_period`, `new_positions`.
- Bundled per-period parquet (`data/period=YYYYQ#/fund13f-YYYYQ#.parquet`) served from GitHub raw with on-demand fetch, ETag revalidation, SHA-256 manifest verification, and a CDN mirror plus stale-cache fallback.
- `fundskit-cli` with `backfill` (quarterly bulk data sets), `nightly-append` (new 13F-HR filings from the EDGAR daily index, merged and deduplicated by accession), `manifest`, and `query`.
- Values normalized to whole US dollars (thousands for filings before 2023, whole dollars from 2023). Holdings identified by CUSIP and issuer name; the `ticker` column is reserved but empty.

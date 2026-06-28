# fundskit

SEC Form 13F institutional holdings for Rust. Served from bundled parquet with on-demand fetch and a local cache. No API keys. Offline after the first query.

## Install

```toml
[dependencies]
fundskit = "0.1.0"
```

To track unreleased changes, depend on the repository directly:

```toml
fundskit = { git = "https://github.com/userFRM/fundskit" }
```

## Quick start

```rust,no_run
#[tokio::main]
async fn main() -> fundskit::Result<()> {
    // Which managers hold a security, by CUSIP, largest position first.
    for h in fundskit::holders_of("037833100").await?.iter().take(5) {
        println!("{} {} shares ${}", h.manager_name, h.shares, h.value_usd);
    }

    // Everything one manager reported, by name or CIK.
    let _berkshire = fundskit::holdings_by_manager("BERKSHIRE HATHAWAY").await?;

    // The most recent report period in the data.
    let _period = fundskit::latest_period().await?;
    Ok(())
}
```

## Client pattern

```rust,no_run
use fundskit::Fundskit;

#[tokio::main]
async fn main() -> fundskit::Result<()> {
    let client = Fundskit::new();

    // Positions a manager opened in a quarter that it did not hold before.
    if let Some(period) = client.latest_period().await? {
        for h in client.new_positions("BERKSHIRE HATHAWAY", period).await? {
            println!("new: {} ${}", h.issuer_name, h.value_usd);
        }
    }
    Ok(())
}
```

Blocking siblings (`holdings_by_manager_blocking`, `holders_of_blocking`, `latest_period_blocking`, `new_positions_blocking`) call the async methods from synchronous code and are safe inside any tokio runtime.

## Identity and tickers

Each holding is identified by `cusip` and `issuer_name`. There is no free, clean CUSIP-to-ticker map, so the `ticker` column is present for forward-compatibility but left empty rather than guessed. The filing manager is identified by `manager_cik` and `manager_name`. Values are normalized to whole US dollars: 13F reported value in thousands for filings made before 2023 and in whole dollars from 2023 on, and both are stored in dollars here.

## CLI

```bash
fundskit-cli backfill --quarters 10          # quarterly bulk, historical
fundskit-cli nightly-append                   # new filings from the daily index
fundskit-cli manifest
fundskit-cli query --manager "BERKSHIRE HATHAWAY"
fundskit-cli query --cusip 037833100
fundskit-cli query --issuer "APPLE"
```

## Data

Sourced from the SEC's Form 13F Data Sets and the EDGAR daily index, which are public domain. One parquet file per report period under `data/period=YYYYQ#/fund13f-YYYYQ#.parquet`, zstd-compressed, one row per reported holding. `data/manifest.json` carries a SHA-256 digest per file. Dates are stored as `i32` `YYYYMMDD`.

The bundled seed has complete coverage for the eight most recent report periods (2024 Q2 through 2026 Q1). Earlier periods are present but partial: they hold only the late filings and amendments that arrived in those recent data-set windows, not the full historical roster. To pull complete older quarters, run `fundskit-cli backfill --quarters N` with a larger `N`; each filing-window data set adds one more complete report period.

The quarterly data set is the historical base. Because it only refreshes once per filing window, a weekday nightly job walks the EDGAR daily index from the last filing date present through today and merges new 13F-HR filings into the right report period, deduplicated by accession, so coverage stays current within the quarter.

## Cache

Fetched parquet is cached on disk (XDG cache dir, e.g. `~/.cache/fundskit/`) with ETag revalidation, so repeat queries are offline. On a network failure the client serves the last good cached copy. Override the origin with `FUNDSKIT_BASE_URL` and the cache location with `FUNDSKIT_CACHE_DIR`.

## API

Full API reference is on [docs.rs](https://docs.rs/fundskit).

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

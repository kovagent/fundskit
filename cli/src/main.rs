//! `fundskit-cli` — build, refresh, and query the bundled 13F-holdings parquet.
//!
//! # Commands
//!
//! ```text
//! fundskit-cli backfill [--quarters 10]
//! fundskit-cli nightly-append
//! fundskit-cli manifest
//! fundskit-cli query --manager "BERKSHIRE HATHAWAY"
//! fundskit-cli query --cusip 037833100
//! fundskit-cli query --issuer "APPLE"
//! ```
//!
//! `backfill` downloads the SEC Form 13F Data Set quarterly ZIPs (the most
//! recent N filing windows) and writes one parquet per report period under
//! `data/period=YYYYQ#/fund13f-YYYYQ#.parquet`. It is the authoritative
//! historical path.
//!
//! `nightly-append` gives same-day coverage: it walks the EDGAR daily index
//! from the last filing date already present through today, parses each new
//! 13F-HR filing, and merges the holdings into the right report-period parquet,
//! deduplicated by accession (idempotent).

mod daily;
mod ingest;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fundskit::{read_holdings, write_holdings, Holding};
use sha2::{Digest, Sha256};

/// Default number of recent filing-window ZIPs to backfill.
const DEFAULT_QUARTERS: usize = 10;

/// Bare `<name> <email>` User-Agent for SEC fetches (parenthetical/URL UAs 403).
fn user_agent() -> String {
    std::env::var("FUNDSKIT_SEC_USER_AGENT")
        .unwrap_or_else(|_| "fundskit contact@example.com".to_string())
}

#[derive(Parser)]
#[command(name = "fundskit-cli", about = "SEC Form 13F institutional holdings")]
struct Cli {
    /// Data directory (default: `<cwd>/data`).
    #[arg(long, env = "FUNDSKIT_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download and rebuild per-period parquet from SEC quarterly data sets.
    Backfill {
        /// Number of most-recent filing-window ZIPs to ingest.
        #[arg(long)]
        quarters: Option<usize>,
    },
    /// Pull new 13F-HR filings from the EDGAR daily index and merge them into
    /// the right report-period parquet, deduplicated by accession.
    NightlyAppend,
    /// Generate `data/manifest.json` with a SHA-256 per parquet file.
    Manifest,
    /// Read bundled parquet and print matching holdings.
    Query {
        /// Filing-manager name substring or CIK.
        #[arg(long)]
        manager: Option<String>,
        /// Exact CUSIP (9 chars).
        #[arg(long)]
        cusip: Option<String>,
        /// Issuer-name substring.
        #[arg(long)]
        issuer: Option<String>,
        /// Maximum rows to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.unwrap_or_else(|| PathBuf::from("data"));

    match cli.cmd {
        Command::Backfill { quarters } => {
            backfill(&data_dir, quarters.unwrap_or(DEFAULT_QUARTERS)).await
        }
        Command::NightlyAppend => nightly_append(&data_dir).await,
        Command::Manifest => write_manifest(&data_dir),
        Command::Query {
            manager,
            cusip,
            issuer,
            limit,
        } => query(&data_dir, manager, cusip, issuer, limit),
    }
}

// ---------------------------------------------------------------------------
// backfill
// ---------------------------------------------------------------------------

async fn backfill(data_dir: &Path, quarters: usize) -> Result<()> {
    let client = http_client()?;
    let zips = discover_zip_urls(&client).await?;
    let take = zips.into_iter().take(quarters).collect::<Vec<_>>();
    eprintln!("backfill: {} ZIPs", take.len());

    // Process one ZIP at a time and merge its rows into the per-period files
    // immediately, so a multi-quarter backfill never holds every quarter's
    // millions of rows in memory at once. A ZIP carries mostly one report
    // period plus stragglers; merge-by-accession makes overlap idempotent.
    for url in &take {
        match fetch_bytes(&client, url).await {
            Ok(bytes) => {
                let rows =
                    ingest::parse_quarter_zip(&bytes).with_context(|| format!("parse {url}"))?;
                eprintln!("  {url}: {} holdings", rows.len());
                write_by_period(data_dir, rows)?;
            }
            Err(e) => eprintln!("  {url}: fetch failed ({e}), skipping"),
        }
    }
    write_manifest(data_dir)
}

/// Scrape the SEC Form 13F Data Sets page for ZIP URLs, newest first. The page
/// lists them in reverse-chronological order, so document order is preserved.
async fn discover_zip_urls(client: &reqwest::Client) -> Result<Vec<String>> {
    const PAGE: &str = "https://www.sec.gov/data-research/sec-markets-data/form-13f-data-sets";
    const PREFIX: &str = "/files/structureddata/data/form-13f-data-sets/";
    let html = fetch_text(client, PAGE)
        .await
        .context("fetch data-sets page")?;
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    // Pull href="…/<name>_form13f.zip" occurrences in page order.
    for chunk in html.split(PREFIX).skip(1) {
        let name: String = chunk
            .chars()
            .take_while(|&c| c != '"' && c != '\'' && c != '>')
            .collect();
        if name.ends_with("_form13f.zip") && seen.insert(name.clone()) {
            urls.push(format!("https://www.sec.gov{PREFIX}{name}"));
        }
    }
    if urls.is_empty() {
        bail!("no 13F data-set ZIP links found on {PAGE}");
    }
    Ok(urls)
}

// ---------------------------------------------------------------------------
// nightly-append: daily-index incremental, merged per report period
// ---------------------------------------------------------------------------

async fn nightly_append(data_dir: &Path) -> Result<()> {
    let today = today_ymd();
    let client = http_client()?;

    // Resume from the day after the latest filing already present anywhere.
    let existing = load_all(data_dir)?;
    let last = existing.iter().map(|r| r.filed_date).max().unwrap_or(0);
    let start = if last > 0 {
        next_day(last)
    } else {
        back_days(today, 7)
    };
    eprintln!(
        "nightly-append: {start} through {today} ({} existing holdings)",
        existing.len()
    );

    let mut fresh = Vec::new();
    let mut day = start;
    while day <= today {
        fresh.extend(daily::ingest_day(&client, day).await?);
        day = next_day(day);
    }
    if fresh.is_empty() {
        eprintln!("no new filings; leaving data unchanged");
        return Ok(());
    }

    // Merge each report period's fresh rows into its parquet, dedup by accession.
    let added = fresh.len();
    for (period, rows) in group_by_period(fresh) {
        let path = period_path(data_dir, period);
        let prior = if path.exists() {
            read_holdings(&std::fs::read(&path)?)?
        } else {
            Vec::new()
        };
        let merged = merge_by_accession(prior, rows);
        write_period_file(&path, period, &merged)?;
    }
    eprintln!("merged {added} fresh holdings");
    write_manifest(data_dir)
}

/// Merge `incoming` into `existing`, deduplicated at the filing (accession)
/// level: every existing row whose accession appears in `incoming` is dropped,
/// then all `incoming` rows are appended. Idempotent.
fn merge_by_accession(existing: Vec<Holding>, incoming: Vec<Holding>) -> Vec<Holding> {
    let incoming_accns: HashSet<&str> = incoming.iter().map(|r| r.accession.as_str()).collect();
    let mut out: Vec<Holding> = existing
        .into_iter()
        .filter(|r| !incoming_accns.contains(r.accession.as_str()))
        .collect();
    out.extend(incoming);
    out
}

// ---------------------------------------------------------------------------
// per-period partitioning + I/O
// ---------------------------------------------------------------------------

/// Partition rows by report period and write/overwrite each period's parquet,
/// merging into any rows already on disk (dedup by accession) so a re-run with
/// overlapping ZIPs is idempotent.
fn write_by_period(data_dir: &Path, rows: Vec<Holding>) -> Result<()> {
    for (period, group) in group_by_period(rows) {
        let path = period_path(data_dir, period);
        let prior = if path.exists() {
            read_holdings(&std::fs::read(&path)?)?
        } else {
            Vec::new()
        };
        let merged = merge_by_accession(prior, group);
        write_period_file(&path, period, &merged)?;
    }
    Ok(())
}

fn group_by_period(rows: Vec<Holding>) -> BTreeMap<i32, Vec<Holding>> {
    let mut by: BTreeMap<i32, Vec<Holding>> = BTreeMap::new();
    for r in rows {
        if r.report_period == 0 {
            continue; // unparseable period; drop rather than mis-file
        }
        by.entry(r.report_period).or_default().push(r);
    }
    by
}

fn write_period_file(path: &Path, period: i32, rows: &[Holding]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let dir = path.parent().context("period path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_holdings(path, rows).with_context(|| format!("write {}", path.display()))?;
    eprintln!(
        "wrote {} ({} holdings, period {period})",
        path.display(),
        rows.len()
    );
    Ok(())
}

/// `data/period=2024Q1/fund13f-2024Q1.parquet` for report period 20240331.
fn period_path(data_dir: &Path, period: i32) -> PathBuf {
    let label = period_label(period);
    data_dir
        .join(format!("period={label}"))
        .join(format!("fund13f-{label}.parquet"))
}

/// `20240331` -> `2024Q1`. Maps each calendar quarter to its quarter-end month.
fn period_label(period: i32) -> String {
    let year = period / 10000;
    let month = (period / 100) % 100;
    let q = ((month - 1) / 3 + 1).clamp(1, 4);
    format!("{year}Q{q}")
}

fn load_all(data_dir: &Path) -> Result<Vec<Holding>> {
    let mut rows = Vec::new();
    for path in find_parquet(data_dir)? {
        rows.extend(read_holdings(&std::fs::read(&path)?)?);
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// SEC fetch
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("build http client")
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client.get(url).send().await.context("send request")?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {url}", resp.status());
    }
    Ok(resp.bytes().await.context("read body")?.to_vec())
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().await.context("send request")?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {url}", resp.status());
    }
    resp.text().await.context("read body")
}

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

/// Write `data/manifest.json` mapping each parquet's path (relative to `data/`)
/// to `sha256:<hex>`. Keys keep the `period=YYYYQ#/` partition prefix so the
/// client resolves the served URL exactly.
fn write_manifest(data_dir: &Path) -> Result<()> {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    for path in find_parquet(data_dir)? {
        let rel = path
            .strip_prefix(data_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&path)?;
        let mut h = Sha256::new();
        h.update(&bytes);
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        entries.insert(rel, format!("sha256:{hex}"));
    }
    let json = serde_json::to_string_pretty(&entries)?;
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("manifest.json");
    std::fs::write(&path, json)?;
    eprintln!("wrote {} ({} files)", path.display(), entries.len());
    Ok(())
}

/// All `*.parquet` under `data/`, one directory level deep (the partition dirs).
fn find_parquet(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !data_dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(data_dir)? {
        let path = entry?.path();
        if path.is_dir() {
            for sub in std::fs::read_dir(&path)? {
                let p = sub?.path();
                if p.extension().and_then(|e| e.to_str()) == Some("parquet") {
                    out.push(p);
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// query (reads local parquet)
// ---------------------------------------------------------------------------

fn query(
    data_dir: &Path,
    manager: Option<String>,
    cusip: Option<String>,
    issuer: Option<String>,
    limit: usize,
) -> Result<()> {
    let mut rows = load_all(data_dir)?;

    if let Some(m) = &manager {
        if let Ok(cik) = m.parse::<u32>() {
            rows.retain(|r| r.manager_cik == cik);
        } else {
            let needle = m.to_lowercase();
            rows.retain(|r| r.manager_name.to_lowercase().contains(&needle));
        }
    }
    if let Some(c) = &cusip {
        let cu = c.trim().to_uppercase();
        rows.retain(|r| r.cusip == cu);
    }
    if let Some(i) = &issuer {
        let needle = i.to_lowercase();
        rows.retain(|r| r.issuer_name.to_lowercase().contains(&needle));
    }
    rows.sort_by_key(|r| {
        (
            std::cmp::Reverse(r.report_period),
            std::cmp::Reverse(r.value_usd),
        )
    });

    println!(
        "{:<8} {:<26} {:<24} {:<10} {:>16} {:>14} {:<4}",
        "period", "manager", "issuer", "cusip", "value_usd", "shares", "p/c"
    );
    for r in rows.iter().take(limit) {
        println!(
            "{:<8} {:<26} {:<24} {:<10} {:>16} {:>14} {:<4}",
            period_label(r.report_period),
            truncate(&r.manager_name, 26),
            truncate(&r.issuer_name, 24),
            r.cusip,
            r.value_usd,
            r.shares,
            r.put_call,
        );
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

// ---------------------------------------------------------------------------
// calendar helpers (system clock; YYYYMMDD math)
// ---------------------------------------------------------------------------

/// Today as a `YYYYMMDD` integer from the system clock.
fn today_ymd() -> i32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    days_to_ymd(secs / 86_400)
}

fn next_day(yyyymmdd: i32) -> i32 {
    days_to_ymd(ymd_to_days(yyyymmdd) + 1)
}

fn back_days(yyyymmdd: i32, n: i64) -> i32 {
    days_to_ymd(ymd_to_days(yyyymmdd) - n)
}

/// `YYYYMMDD` -> days since 1970-01-01 (Hinnant's days-from-civil).
fn ymd_to_days(d: i32) -> i64 {
    let y = (d / 10000) as i64;
    let m = ((d / 100) % 100) as i64;
    let day = (d % 100) as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 -> `YYYYMMDD` (Hinnant's civil-from-days).
fn days_to_ymd(days: i64) -> i32 {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y * 10000 + m * 100 + d) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ymd_days_round_trip() {
        assert_eq!(ymd_to_days(19700101), 0);
        assert_eq!(days_to_ymd(0), 19700101);
        assert_eq!(next_day(20240228), 20240229);
        assert_eq!(next_day(20251231), 20260101);
        assert_eq!(days_to_ymd(ymd_to_days(20260628)), 20260628);
        assert_eq!(back_days(20260101, 1), 20251231);
    }

    #[test]
    fn period_labels() {
        assert_eq!(period_label(20240331), "2024Q1");
        assert_eq!(period_label(20240630), "2024Q2");
        assert_eq!(period_label(20240930), "2024Q3");
        assert_eq!(period_label(20241231), "2024Q4");
    }

    #[test]
    fn period_path_layout() {
        let p = period_path(Path::new("data"), 20240331);
        assert!(p.ends_with("period=2024Q1/fund13f-2024Q1.parquet"));
    }

    #[test]
    fn merge_dedups_by_accession() {
        let mk = |accn: &str, value: i64| Holding {
            report_period: 20240331,
            filed_date: 20240514,
            accession: accn.into(),
            manager_cik: 1,
            manager_name: "M".into(),
            issuer_name: "X".into(),
            cusip: "037833100".into(),
            ticker: String::new(),
            title_of_class: "COM".into(),
            value_usd: value,
            shares: 1,
            share_type: "SH".into(),
            put_call: String::new(),
            discretion: "SOLE".into(),
        };
        let existing = vec![mk("acc-1", 100), mk("acc-2", 200)];
        let incoming = vec![mk("acc-1", 999), mk("acc-3", 300)];
        let merged = merge_by_accession(existing, incoming);
        assert_eq!(merged.len(), 3); // acc-1 replaced, acc-2 kept, acc-3 added
        let one: Vec<_> = merged.iter().filter(|r| r.accession == "acc-1").collect();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].value_usd, 999); // incoming won

        let again = merge_by_accession(merged.clone(), vec![mk("acc-1", 999), mk("acc-3", 300)]);
        assert_eq!(again.len(), 3); // idempotent
    }

    #[test]
    fn discover_parses_hrefs_in_order() {
        // group_by_period drops period 0.
        let rows = vec![Holding {
            report_period: 0,
            filed_date: 20240514,
            accession: "z".into(),
            manager_cik: 1,
            manager_name: "M".into(),
            issuer_name: "X".into(),
            cusip: "c".into(),
            ticker: String::new(),
            title_of_class: "COM".into(),
            value_usd: 1,
            shares: 1,
            share_type: "SH".into(),
            put_call: String::new(),
            discretion: "SOLE".into(),
        }];
        assert!(group_by_period(rows).is_empty());
    }
}

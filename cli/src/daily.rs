//! Same-day incremental ingest from the EDGAR daily index + per-filing 13F XML.
//!
//! The quarterly Form 13F Data Set only refreshes once per filing window, so it
//! is stale within a quarter. The daily index lists each 13F-HR as it is filed.
//! This module:
//!
//! 1. fetches `form.{YYYYMMDD}.idx` and keeps the `13F-HR` / `13F-HR/A` rows,
//! 2. for each, reads the filing's `primary_doc.xml` (manager, period) and its
//!    information-table XML (the holdings),
//! 3. emits the same [`Holding`] shape the quarterly TSV path produces.
//!
//! XML element names carry a namespace prefix that varies by filer
//! (`ns1:infoTable`, `infoTable`, …), so both parsers match on the *local*
//! name (the part after the last `:`) rather than the literal tag.

use std::time::Duration;

use anyhow::{Context, Result};
use fundskit::Holding;
use quick_xml::events::Event;
use quick_xml::Reader;

/// SEC asks for <= 10 requests/second. A small per-request pause with serial
/// fetches keeps us well under that.
const REQUEST_PAUSE: Duration = Duration::from_millis(150);

/// SEC switched 13F value reporting to whole dollars for filings on/after
/// 2023-01-01; earlier filings report value in thousands. The daily path only
/// sees current filings, but the normalization is kept explicit for parity with
/// the quarterly path.
const WHOLE_DOLLAR_FROM: i32 = 20230101;

/// One filing referenced by a daily-index row.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    pub form_type: String,
    pub cik: u32,
    pub accession: String,
}

/// Parse a `form.{YYYYMMDD}.idx` body, keeping only 13F holdings reports.
///
/// The form type is the first whitespace token and the path is the last token;
/// CIK is recovered from the path so column alignment is never load-bearing.
pub fn parse_daily_index(body: &str) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        // 13F rows start with the form type "13F-…", i.e. a digit.
        if !trimmed.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let form = trimmed.split_whitespace().next().unwrap_or("");
        if !is_holdings_form(form) {
            continue;
        }
        let Some(path) = trimmed.split_whitespace().last() else {
            continue;
        };
        let Some((cik, accession)) = parse_filing_path(path) else {
            continue;
        };
        out.push(IndexEntry {
            form_type: form.to_string(),
            cik,
            accession,
        });
    }
    out
}

fn is_holdings_form(form: &str) -> bool {
    matches!(form, "13F-HR" | "13F-HR/A")
}

/// `edgar/data/1663719/0001709164-26-000096.txt` -> (1663719, accession).
fn parse_filing_path(path: &str) -> Option<(u32, String)> {
    let rest = path.strip_prefix("edgar/data/")?;
    let (cik, file) = rest.split_once('/')?;
    let accession = file.strip_suffix(".txt")?;
    if accession.len() != 20 || accession.matches('-').count() != 2 {
        return None;
    }
    Some((cik.parse().ok()?, accession.to_string()))
}

// ---------------------------------------------------------------------------
// primary_doc.xml -> cover metadata
// ---------------------------------------------------------------------------

/// Cover-page fields pulled from `primary_doc.xml`.
pub struct Cover {
    pub manager_name: String,
    pub report_period: i32,
    pub is_holdings: bool,
}

/// Strip a namespace prefix: `ns1:infoTable` -> `infoTable`.
fn local_name(raw: &[u8]) -> &[u8] {
    match raw.iter().rposition(|&b| b == b':') {
        Some(i) => &raw[i + 1..],
        None => raw,
    }
}

/// Parse `primary_doc.xml` for the filing manager, report period, and whether
/// it is a holdings report. `submissionType` and `periodOfReport` sit in the
/// header; the manager name is the first `<name>` inside `<filingManager>`
/// (a second `<name>` appears in the signature block and must be ignored).
pub fn parse_primary_doc(xml: &str) -> Cover {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut cur: Option<&'static str> = None;

    let mut submission_type = String::new();
    let mut period_raw = String::new();
    let mut manager_name = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref()).to_vec();
                cur = match name.as_slice() {
                    b"submissionType" => Some("submissionType"),
                    b"periodOfReport" => Some("periodOfReport"),
                    b"name" if path.iter().any(|p| p == b"filingManager") => {
                        if manager_name.is_empty() {
                            Some("name")
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                path.push(name);
            }
            Ok(Event::Text(t)) => {
                if let Some(field) = cur.take() {
                    let val = t.decode().unwrap_or_default().trim().to_string();
                    match field {
                        "submissionType" => submission_type = val,
                        "periodOfReport" => period_raw = val,
                        "name" => manager_name = val,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => {
                path.pop();
                cur = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    Cover {
        manager_name,
        report_period: parse_mdy(&period_raw),
        is_holdings: submission_type == "13F-HR" || submission_type == "13F-HR/A",
    }
}

// ---------------------------------------------------------------------------
// information-table XML -> Holding rows
// ---------------------------------------------------------------------------

/// Fields accumulated for the current `<infoTable>` element.
#[derive(Default)]
struct Entry {
    issuer: String,
    title: String,
    cusip: String,
    value: i64,
    shares: i64,
    share_type: String,
    put_call: String,
    discretion: String,
}

/// Parse an information-table XML body into holdings. The submission metadata
/// (manager, period, dates) comes from the caller; the table itself carries
/// only the per-position figures.
pub fn parse_info_table(
    xml: &str,
    accession: &str,
    cik: u32,
    cover: &Cover,
    filed_date: i32,
) -> Vec<Holding> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut entry: Option<Entry> = None;
    let mut field: Option<&'static str> = None;
    let mut rows = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref()).to_vec();
                match name.as_slice() {
                    b"infoTable" => entry = Some(Entry::default()),
                    b"nameOfIssuer" => field = Some("issuer"),
                    b"titleOfClass" => field = Some("title"),
                    b"cusip" => field = Some("cusip"),
                    b"value" => field = Some("value"),
                    b"sshPrnamt" => field = Some("shares"),
                    b"sshPrnamtType" => field = Some("share_type"),
                    b"putCall" => field = Some("put_call"),
                    b"investmentDiscretion" => field = Some("discretion"),
                    _ => field = None,
                }
                path.push(name);
            }
            Ok(Event::Text(t)) => {
                if let (Some(f), Some(en)) = (field.take(), entry.as_mut()) {
                    let v = t.decode().unwrap_or_default().trim().to_string();
                    match f {
                        "issuer" => en.issuer = v,
                        "title" => en.title = v,
                        "cusip" => en.cusip = v.to_uppercase(),
                        "value" => en.value = v.parse().unwrap_or(0),
                        "shares" => en.shares = v.parse().unwrap_or(0),
                        "share_type" => en.share_type = v,
                        "put_call" => en.put_call = v,
                        "discretion" => en.discretion = v,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == b"infoTable" {
                    if let Some(en) = entry.take() {
                        let value_usd = if filed_date < WHOLE_DOLLAR_FROM {
                            en.value.saturating_mul(1000)
                        } else {
                            en.value
                        };
                        rows.push(Holding {
                            report_period: cover.report_period,
                            filed_date,
                            accession: accession.to_string(),
                            manager_cik: cik,
                            manager_name: cover.manager_name.clone(),
                            issuer_name: en.issuer,
                            cusip: en.cusip,
                            ticker: String::new(),
                            title_of_class: en.title,
                            value_usd,
                            shares: en.shares,
                            share_type: en.share_type,
                            put_call: en.put_call,
                            discretion: en.discretion,
                        });
                    }
                }
                path.pop();
                field = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    rows
}

// ---------------------------------------------------------------------------
// async fetch orchestration
// ---------------------------------------------------------------------------

/// Fetch and parse every 13F-HR filing in the daily index for `date`
/// (YYYYMMDD). Filings that fail to fetch or parse are counted and skipped,
/// never fatal.
pub async fn ingest_day(client: &reqwest::Client, date: i32) -> Result<Vec<Holding>> {
    let (y, m, _d) = split_ymd(date);
    let q = (m - 1) / 3 + 1;
    let url =
        format!("https://www.sec.gov/Archives/edgar/daily-index/{y}/QTR{q}/form.{date:08}.idx");
    let body = match get_text(client, &url).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            eprintln!("{date}: no daily index (weekend/holiday), skipping");
            return Ok(Vec::new());
        }
        Err(e) => {
            eprintln!("{date}: daily index fetch failed ({e}), skipping");
            return Ok(Vec::new());
        }
    };

    let entries = parse_daily_index(&body);
    let mut rows = Vec::new();
    let (mut ok, mut failed) = (0u32, 0u32);
    for e in &entries {
        match fetch_filing_rows(client, e, date).await {
            Ok(mut r) => {
                ok += 1;
                rows.append(&mut r);
            }
            Err(err) => {
                failed += 1;
                eprintln!("  skip {}: {err}", e.accession);
            }
        }
        tokio::time::sleep(REQUEST_PAUSE).await;
    }
    eprintln!(
        "{date}: {} 13F filings, {ok} parsed, {failed} skipped, {} holdings",
        entries.len(),
        rows.len()
    );
    Ok(rows)
}

/// Locate and fetch one filing's primary doc + information table, then parse.
async fn fetch_filing_rows(
    client: &reqwest::Client,
    entry: &IndexEntry,
    filed_date: i32,
) -> Result<Vec<Holding>> {
    let nodash: String = entry.accession.chars().filter(|c| *c != '-').collect();
    let base = format!(
        "https://www.sec.gov/Archives/edgar/data/{}/{}",
        entry.cik, nodash
    );

    let index = get_text(client, &format!("{base}/index.json"))
        .await?
        .context("filing index.json 404")?;
    let (primary, info_name) = pick_xml_files(&index).context("13F xml not found in filing")?;

    let primary_xml = get_text(client, &format!("{base}/{primary}"))
        .await?
        .context("primary_doc.xml 404")?;
    let cover = parse_primary_doc(&primary_xml);
    if !cover.is_holdings {
        return Ok(Vec::new()); // notice or non-holdings amendment
    }

    let info_xml = get_text(client, &format!("{base}/{info_name}"))
        .await?
        .context("information table xml 404")?;
    Ok(parse_info_table(
        &info_xml,
        &entry.accession,
        entry.cik,
        &cover,
        filed_date,
    ))
}

/// From a filing `index.json`, return `(primary_doc, information_table)` file
/// names. The primary doc is `primary_doc.xml`; the information table is the
/// other `.xml` (its name varies, e.g. `form13fInfoTable.xml`).
fn pick_xml_files(index_json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(index_json).ok()?;
    let items = v.get("directory")?.get("item")?.as_array()?;
    let xmls: Vec<String> = items
        .iter()
        .filter_map(|i| i.get("name")?.as_str().map(str::to_string))
        .filter(|n| n.ends_with(".xml"))
        .collect();
    let primary = xmls.iter().find(|n| *n == "primary_doc.xml")?.clone();
    let info = xmls.iter().find(|n| *n != "primary_doc.xml")?.clone();
    Some((primary, info))
}

/// GET a URL as text. `Ok(None)` for 404; retries once on 429/5xx.
async fn get_text(client: &reqwest::Client, url: &str) -> Result<Option<String>> {
    for attempt in 0..2 {
        let resp = client.get(url).send().await.context("send")?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            return Ok(Some(resp.text().await.context("body")?));
        }
        if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            && attempt == 0
        {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            continue;
        }
        anyhow::bail!("HTTP {status} for {url}");
    }
    anyhow::bail!("exhausted retries for {url}")
}

// ---------------------------------------------------------------------------
// date helpers
// ---------------------------------------------------------------------------

fn split_ymd(d: i32) -> (i32, i32, i32) {
    (d / 10000, (d / 100) % 100, d % 100)
}

/// Parse `primary_doc.xml`'s `MM-DD-YYYY` date (e.g. `09-30-2021`) to YYYYMMDD.
fn parse_mdy(s: &str) -> i32 {
    let s = s.trim();
    let mut p = s.split('-');
    let (Some(m), Some(d), Some(y)) = (p.next(), p.next(), p.next()) else {
        return 0;
    };
    let (Ok(m), Ok(d), Ok(y)) = (m.parse::<i32>(), d.parse::<i32>(), y.parse::<i32>()) else {
        return 0;
    };
    if m == 0 || d == 0 || y == 0 {
        return 0;
    }
    y * 10000 + m * 100 + d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_path() {
        assert_eq!(
            parse_filing_path("edgar/data/1663719/0001709164-26-000096.txt"),
            Some((1663719, "0001709164-26-000096".to_string()))
        );
        assert_eq!(parse_filing_path("edgar/data/5/bad.txt"), None);
    }

    #[test]
    fn keeps_only_13f_holdings_forms() {
        let body = "\
Form Type   Company   CIK   Date Filed   File Name
----------
13F-HR           Acme Capital   123   20260602   edgar/data/123/0001000000-26-000001.txt
10-K             Beta Co        456   20260602   edgar/data/456/0001000000-26-000002.txt
13F-HR/A         Gamma LLC      789   20260602   edgar/data/789/0001000000-26-000003.txt
13F-NT           Delta Notice   111   20260602   edgar/data/111/0001000000-26-000004.txt
";
        let e = parse_daily_index(body);
        assert_eq!(e.len(), 2, "HR and HR/A kept; NT and 10-K dropped");
        assert_eq!(e[0].form_type, "13F-HR");
        assert_eq!(e[1].form_type, "13F-HR/A");
    }

    #[test]
    fn local_name_strips_prefix() {
        assert_eq!(local_name(b"ns1:infoTable"), b"infoTable");
        assert_eq!(local_name(b"infoTable"), b"infoTable");
        assert_eq!(local_name(b"com:street1"), b"street1");
    }

    #[test]
    fn mdy_parses() {
        assert_eq!(parse_mdy("09-30-2021"), 20210930);
        assert_eq!(parse_mdy("12-31-2025"), 20251231);
        assert_eq!(parse_mdy(""), 0);
    }

    #[test]
    fn primary_doc_takes_filing_manager_name_not_signature() {
        let xml = r#"<?xml version="1.0"?>
<edgarSubmission>
  <headerData><filerInfo>
    <filer><credentials><cik>0002134841</cik></credentials></filer>
    <periodOfReport>09-30-2021</periodOfReport>
  </filerInfo></headerData>
  <formData>
    <coverPage>
      <submissionType>13F-HR</submissionType>
      <filingManager><name>First Nebraska Trust Co</name></filingManager>
    </coverPage>
    <signatureBlock><name>Scott A. Wendt</name></signatureBlock>
  </formData>
</edgarSubmission>"#;
        let c = parse_primary_doc(xml);
        assert_eq!(c.manager_name, "First Nebraska Trust Co");
        assert_eq!(c.report_period, 20210930);
        assert!(c.is_holdings);
    }

    #[test]
    fn info_table_namespaced_parses() {
        let cover = Cover {
            manager_name: "First Nebraska Trust Co".into(),
            report_period: 20210930,
            is_holdings: true,
        };
        let xml = r#"<?xml version="1.0"?>
<ns1:informationTable xmlns:ns1="http://www.sec.gov/edgar/document/thirteenf/informationtable">
  <ns1:infoTable>
    <ns1:nameOfIssuer>ABBOTT LABS</ns1:nameOfIssuer>
    <ns1:titleOfClass>COM</ns1:titleOfClass>
    <ns1:cusip>002824100</ns1:cusip>
    <ns1:value>799621</ns1:value>
    <ns1:shrsOrPrnAmt><ns1:sshPrnamt>6769</ns1:sshPrnamt><ns1:sshPrnamtType>SH</ns1:sshPrnamtType></ns1:shrsOrPrnAmt>
    <ns1:investmentDiscretion>SOLE</ns1:investmentDiscretion>
  </ns1:infoTable>
  <ns1:infoTable>
    <ns1:nameOfIssuer>SPY PUT</ns1:nameOfIssuer>
    <ns1:titleOfClass>PUT</ns1:titleOfClass>
    <ns1:cusip>78462f103</ns1:cusip>
    <ns1:value>123456</ns1:value>
    <ns1:shrsOrPrnAmt><ns1:sshPrnamt>1000</ns1:sshPrnamt><ns1:sshPrnamtType>SH</ns1:sshPrnamtType></ns1:shrsOrPrnAmt>
    <ns1:putCall>Put</ns1:putCall>
    <ns1:investmentDiscretion>SOLE</ns1:investmentDiscretion>
  </ns1:infoTable>
</ns1:informationTable>"#;
        // filed 2026 -> whole dollars, no x1000.
        let rows = parse_info_table(xml, "0002134841-26-000057", 2134841, &cover, 20260625);
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert_eq!(r.issuer_name, "ABBOTT LABS");
        assert_eq!(r.cusip, "002824100");
        assert_eq!(r.value_usd, 799621);
        assert_eq!(r.shares, 6769);
        assert_eq!(r.share_type, "SH");
        assert_eq!(r.manager_cik, 2134841);
        assert_eq!(r.manager_name, "First Nebraska Trust Co");
        assert_eq!(r.report_period, 20210930);
        assert!(r.put_call.is_empty());
        assert_eq!(rows[1].put_call, "Put");
        assert_eq!(rows[1].cusip, "78462F103"); // uppercased
    }
}

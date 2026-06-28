//! Parse the SEC Form 13F Data Set quarterly ZIP into [`Holding`] rows.
//!
//! The ZIP carries tab-separated tables that join on `ACCESSION_NUMBER`:
//!
//! - `SUBMISSION.tsv`  — accession, filing date, submission type, CIK, period.
//! - `COVERPAGE.tsv`   — accession, filing-manager name.
//! - `INFOTABLE.tsv`   — one row per holding (issuer, cusip, value, shares, …).
//!
//! Only `13F-HR` and `13F-HR/A` submissions carry holdings (`13F-NT` is a
//! notice with none). Value is normalized to whole US dollars: the SEC reported
//! it in thousands for filings made before 2023 and in whole dollars from 2023
//! on, so a pre-2023 filing's value is multiplied by 1000.

use std::collections::HashMap;
use std::io::Read;

use anyhow::{Context, Result};
use fundskit::Holding;

/// SEC switched 13F value reporting from thousands to whole dollars for filings
/// made on or after 2023-01-01. Earlier filings store value in thousands.
const WHOLE_DOLLAR_FROM: i32 = 20230101;

/// One submission's metadata, keyed by accession.
struct Submission {
    filed_date: i32,
    cik: u32,
    report_period: i32,
    is_holdings: bool,
}

/// Parse every holding in a quarterly ZIP. Returns rows tagged with their
/// report period (which may span several quarters: a ZIP holds mostly one
/// period plus late filings and amendments for older periods).
pub fn parse_quarter_zip(bytes: &[u8]) -> Result<Vec<Holding>> {
    let reader = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(reader).context("open zip")?;

    let submission = read_named(&mut zip, "SUBMISSION.tsv")?;
    let coverpage = read_named(&mut zip, "COVERPAGE.tsv")?;
    let infotable = read_named(&mut zip, "INFOTABLE.tsv")?;

    let submissions = parse_submission(&submission);
    let managers = parse_coverpage(&coverpage);
    Ok(parse_infotable(&infotable, &submissions, &managers))
}

/// Read a member by basename. Some quarterly ZIPs put the TSVs at the root,
/// others nest them under a `<window>_form13f/` directory, so match the entry
/// whose path ends with `/<name>` or equals `<name>`.
fn read_named(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Result<String> {
    let suffix = format!("/{name}");
    let idx = (0..zip.len())
        .find(|&i| {
            zip.by_index(i)
                .map(|f| {
                    let n = f.name();
                    n == name || n.ends_with(&suffix)
                })
                .unwrap_or(false)
        })
        .with_context(|| format!("{name} not in zip"))?;
    let mut file = zip.by_index(idx).with_context(|| format!("open {name}"))?;
    let mut s = String::new();
    file.read_to_string(&mut s)
        .with_context(|| format!("read {name}"))?;
    Ok(s)
}

/// `ACCESSION_NUMBER FILING_DATE SUBMISSIONTYPE CIK PERIODOFREPORT`.
fn parse_submission(tsv: &str) -> HashMap<String, Submission> {
    let mut out = HashMap::new();
    let cols = match header_index(tsv) {
        Some(c) => c,
        None => return out,
    };
    for line in tsv.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        let Some(accn) = col(&f, &cols, "ACCESSION_NUMBER") else {
            continue;
        };
        let kind = col(&f, &cols, "SUBMISSIONTYPE").unwrap_or("");
        out.insert(
            accn.to_string(),
            Submission {
                filed_date: parse_dmy(col(&f, &cols, "FILING_DATE").unwrap_or("")),
                cik: col(&f, &cols, "CIK")
                    .unwrap_or("")
                    .trim()
                    .parse()
                    .unwrap_or(0),
                report_period: parse_dmy(col(&f, &cols, "PERIODOFREPORT").unwrap_or("")),
                is_holdings: kind == "13F-HR" || kind == "13F-HR/A",
            },
        );
    }
    out
}

/// `ACCESSION_NUMBER … FILINGMANAGER_NAME …` — keep accession -> manager name.
fn parse_coverpage(tsv: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let cols = match header_index(tsv) {
        Some(c) => c,
        None => return out,
    };
    for line in tsv.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        if let Some(accn) = col(&f, &cols, "ACCESSION_NUMBER") {
            let name = col(&f, &cols, "FILINGMANAGER_NAME").unwrap_or("").trim();
            out.insert(accn.to_string(), name.to_string());
        }
    }
    out
}

/// One row per holding, joined to its submission + manager by accession.
/// Holdings whose submission is missing or is a notice (`13F-NT`) are dropped.
fn parse_infotable(
    tsv: &str,
    submissions: &HashMap<String, Submission>,
    managers: &HashMap<String, String>,
) -> Vec<Holding> {
    let cols = match header_index(tsv) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in tsv.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        let Some(accn) = col(&f, &cols, "ACCESSION_NUMBER") else {
            continue;
        };
        let Some(sub) = submissions.get(accn) else {
            continue;
        };
        if !sub.is_holdings {
            continue;
        }
        let raw_value: i64 = col(&f, &cols, "VALUE")
            .unwrap_or("")
            .trim()
            .parse()
            .unwrap_or(0);
        let value_usd = if sub.filed_date < WHOLE_DOLLAR_FROM {
            raw_value.saturating_mul(1000)
        } else {
            raw_value
        };
        out.push(Holding {
            report_period: sub.report_period,
            filed_date: sub.filed_date,
            accession: accn.to_string(),
            manager_cik: sub.cik,
            manager_name: managers.get(accn).cloned().unwrap_or_default(),
            issuer_name: col(&f, &cols, "NAMEOFISSUER")
                .unwrap_or("")
                .trim()
                .to_string(),
            cusip: col(&f, &cols, "CUSIP").unwrap_or("").trim().to_uppercase(),
            ticker: String::new(),
            title_of_class: col(&f, &cols, "TITLEOFCLASS")
                .unwrap_or("")
                .trim()
                .to_string(),
            value_usd,
            shares: col(&f, &cols, "SSHPRNAMT")
                .unwrap_or("")
                .trim()
                .parse()
                .unwrap_or(0),
            share_type: col(&f, &cols, "SSHPRNAMTTYPE")
                .unwrap_or("")
                .trim()
                .to_string(),
            put_call: col(&f, &cols, "PUTCALL").unwrap_or("").trim().to_string(),
            discretion: col(&f, &cols, "INVESTMENTDISCRETION")
                .unwrap_or("")
                .trim()
                .to_string(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// TSV column resolution (header-name based, not positional)
// ---------------------------------------------------------------------------

/// Map column name -> index from the first (header) line.
fn header_index(tsv: &str) -> Option<HashMap<String, usize>> {
    let header = tsv.lines().next()?;
    Some(
        header
            .split('\t')
            .enumerate()
            .map(|(i, name)| (name.trim().to_string(), i))
            .collect(),
    )
}

fn col<'a>(fields: &[&'a str], cols: &HashMap<String, usize>, name: &str) -> Option<&'a str> {
    cols.get(name).and_then(|&i| fields.get(i).copied())
}

// ---------------------------------------------------------------------------
// Date parsing: `DD-MON-YYYY` -> YYYYMMDD i32
// ---------------------------------------------------------------------------

/// Parse the SEC data-set date format `31-MAR-2023` into a `YYYYMMDD` integer.
/// Returns 0 on any parse failure.
pub fn parse_dmy(s: &str) -> i32 {
    let s = s.trim();
    let mut parts = s.split('-');
    let (Some(d), Some(m), Some(y)) = (parts.next(), parts.next(), parts.next()) else {
        return 0;
    };
    let day: i32 = d.parse().unwrap_or(0);
    let year: i32 = y.parse().unwrap_or(0);
    let month = month_num(m);
    if day == 0 || year == 0 || month == 0 {
        return 0;
    }
    year * 10000 + month * 100 + day
}

fn month_num(m: &str) -> i32 {
    match m.to_ascii_uppercase().as_str() {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmy_parses() {
        assert_eq!(parse_dmy("31-MAR-2023"), 20230331);
        assert_eq!(parse_dmy("01-jan-2024"), 20240101);
        assert_eq!(parse_dmy("31-DEC-2020"), 20201231);
        assert_eq!(parse_dmy(""), 0);
        assert_eq!(parse_dmy("garbage"), 0);
    }

    #[test]
    fn header_and_col_lookup() {
        let tsv = "A\tB\tC\n1\t2\t3\n";
        let cols = header_index(tsv).unwrap();
        let row: Vec<&str> = tsv.lines().nth(1).unwrap().split('\t').collect();
        assert_eq!(col(&row, &cols, "B"), Some("2"));
        assert_eq!(col(&row, &cols, "MISSING"), None);
    }

    #[test]
    fn joins_and_normalizes_value_scale() {
        let submission = "ACCESSION_NUMBER\tFILING_DATE\tSUBMISSIONTYPE\tCIK\tPERIODOFREPORT\n\
            acc-old\t30-DEC-2022\t13F-HR\t1067983\t31-DEC-2021\n\
            acc-new\t14-MAY-2024\t13F-HR\t1067983\t31-MAR-2024\n\
            acc-nt\t14-MAY-2024\t13F-NT\t999\t31-MAR-2024\n";
        let coverpage = "ACCESSION_NUMBER\tFILINGMANAGER_NAME\n\
            acc-old\tBERKSHIRE HATHAWAY INC\n\
            acc-new\tBERKSHIRE HATHAWAY INC\n\
            acc-nt\tNOTICE FILER\n";
        let infotable = "ACCESSION_NUMBER\tNAMEOFISSUER\tTITLEOFCLASS\tCUSIP\tVALUE\tSSHPRNAMT\tSSHPRNAMTTYPE\tPUTCALL\tINVESTMENTDISCRETION\n\
            acc-old\tAPPLE INC\tCOM\t037833100\t18699\t102738\tSH\t\tSOLE\n\
            acc-new\tAPPLE INC\tCOM\t037833100\t135360000000\t789368450\tSH\t\tSOLE\n\
            acc-nt\tSOMETHING\tCOM\t000000000\t1\t1\tSH\t\tSOLE\n";

        let subs = parse_submission(submission);
        let mgrs = parse_coverpage(coverpage);
        let rows = parse_infotable(infotable, &subs, &mgrs);

        // The 13F-NT holding is dropped; two HR holdings remain.
        assert_eq!(rows.len(), 2);

        let old = rows.iter().find(|r| r.accession == "acc-old").unwrap();
        // Filed 2022 -> value was thousands -> x1000.
        assert_eq!(old.value_usd, 18_699_000);
        assert_eq!(old.report_period, 20211231);
        assert_eq!(old.manager_name, "BERKSHIRE HATHAWAY INC");

        let new = rows.iter().find(|r| r.accession == "acc-new").unwrap();
        // Filed 2024 -> already whole dollars.
        assert_eq!(new.value_usd, 135_360_000_000);
        assert_eq!(new.shares, 789_368_450);
        assert_eq!(new.report_period, 20240331);
    }
}

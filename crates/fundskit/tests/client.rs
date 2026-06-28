//! End-to-end: serve a manifest + a real parquet shard, then confirm the
//! client fetches, reads, and filters it.

use fundskit::{write_holdings, Fundskit, Holding};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn h(manager: &str, cik: u32, cusip: &str, issuer: &str, period: i32, value: i64) -> Holding {
    Holding {
        report_period: period,
        filed_date: period + 44,
        accession: format!("{cik}-{period}"),
        manager_cik: cik,
        manager_name: manager.into(),
        issuer_name: issuer.into(),
        cusip: cusip.into(),
        ticker: String::new(),
        title_of_class: "COM".into(),
        value_usd: value,
        shares: value / 100,
        share_type: "SH".into(),
        put_call: String::new(),
        discretion: "SOLE".into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn client_reads_served_parquet() {
    let dir = tempfile::TempDir::new().unwrap();
    let shard_path = dir.path().join("fund13f-2024Q1.parquet");
    let rows = vec![
        h(
            "BERKSHIRE HATHAWAY INC",
            1067983,
            "037833100",
            "APPLE INC",
            20240331,
            135_000_000_000,
        ),
        h(
            "BERKSHIRE HATHAWAY INC",
            1067983,
            "060505104",
            "BANK AMER CORP",
            20240331,
            39_000_000_000,
        ),
        h(
            "RENAISSANCE TECHNOLOGIES",
            1037389,
            "037833100",
            "APPLE INC",
            20240331,
            500_000_000,
        ),
    ];
    write_holdings(&shard_path, &rows).unwrap();
    let parquet = std::fs::read(&shard_path).unwrap();
    let digest = sha256_hex(&parquet);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/manifest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"period=2024Q1/fund13f-2024Q1.parquet":"sha256:{digest}"}}"#
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/period=2024Q1/fund13f-2024Q1.parquet"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(parquet))
        .mount(&server)
        .await;

    let cache = tempfile::TempDir::new().unwrap();
    let client = Fundskit::new()
        .with_base_url(server.uri())
        .with_cache_dir(cache.path().to_path_buf())
        .with_mirror_url(None);

    // Holders of Apple by CUSIP: two managers.
    let apple = client.holders_of("037833100").await.unwrap();
    assert_eq!(apple.len(), 2);
    assert_eq!(apple[0].value_usd, 135_000_000_000, "largest first");

    // By issuer-name substring works too.
    let by_name = client.holders_of("apple").await.unwrap();
    assert_eq!(by_name.len(), 2);

    // All of one manager's positions, by name and by CIK.
    let brk = client.holdings_by_manager("BERKSHIRE").await.unwrap();
    assert_eq!(brk.len(), 2);
    let brk_cik = client.holdings_by_manager("1067983").await.unwrap();
    assert_eq!(brk_cik.len(), 2);

    assert_eq!(client.latest_period().await.unwrap(), Some(20240331));
}

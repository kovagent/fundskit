//! Parquet reader/writer for 13F holding rows.
//!
//! # File layout
//!
//! One row per holding. Columns, in order:
//!
//! ```text
//! report_period Int32(YYYYMMDD), filed_date Int32(YYYYMMDD), accession Utf8,
//! manager_cik UInt32, manager_name Utf8, issuer_name Utf8, cusip Utf8,
//! ticker Utf8, title_of_class Utf8, value_usd Int64, shares Int64,
//! share_type Utf8, put_call Utf8, discretion Utf8
//! ```
//!
//! Dates are plain `i32` `YYYYMMDD` integers, not Arrow `Date32`, so a consumer
//! never needs a calendar library to compare or bucket them. `value_usd` and
//! `shares` are `i64`: 13F values are whole dollars and share counts are
//! integers, and a large fund's total can exceed `i32`.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, Int32Array, Int64Array, StringArray, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::error::{Error, Result};
use crate::record::Holding;

const ROW_GROUP: usize = 100_000;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// The bundled-parquet schema, bound field by field. Every column non-null;
/// the writer fills empty strings rather than nulls so the read path can reject
/// any unexpected null as corruption.
fn holding_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("report_period", DataType::Int32, false),
        Field::new("filed_date", DataType::Int32, false),
        Field::new("accession", DataType::Utf8, false),
        Field::new("manager_cik", DataType::UInt32, false),
        Field::new("manager_name", DataType::Utf8, false),
        Field::new("issuer_name", DataType::Utf8, false),
        Field::new("cusip", DataType::Utf8, false),
        Field::new("ticker", DataType::Utf8, false),
        Field::new("title_of_class", DataType::Utf8, false),
        Field::new("value_usd", DataType::Int64, false),
        Field::new("shares", DataType::Int64, false),
        Field::new("share_type", DataType::Utf8, false),
        Field::new("put_call", DataType::Utf8, false),
        Field::new("discretion", DataType::Utf8, false),
    ]))
}

fn writer_props() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(3).expect("valid zstd level"),
        ))
        .set_max_row_group_row_count(Some(ROW_GROUP))
        .build()
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Write `rows` to a parquet file at `path` (creates or overwrites).
pub fn write_holdings(path: &Path, rows: &[Holding]) -> Result<()> {
    let schema = holding_schema();
    let file = fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(writer_props()))?;
    for chunk in rows.chunks(ROW_GROUP) {
        writer.write(&batch_of(&schema, chunk)?)?;
    }
    writer.close()?;
    Ok(())
}

fn batch_of(schema: &Arc<Schema>, rows: &[Holding]) -> Result<RecordBatch> {
    let report_period: Int32Array = rows.iter().map(|r| Some(r.report_period)).collect();
    let filed_date: Int32Array = rows.iter().map(|r| Some(r.filed_date)).collect();
    let accession: StringArray = rows.iter().map(|r| Some(r.accession.as_str())).collect();
    let manager_cik: UInt32Array = rows.iter().map(|r| Some(r.manager_cik)).collect();
    let manager_name: StringArray = rows.iter().map(|r| Some(r.manager_name.as_str())).collect();
    let issuer_name: StringArray = rows.iter().map(|r| Some(r.issuer_name.as_str())).collect();
    let cusip: StringArray = rows.iter().map(|r| Some(r.cusip.as_str())).collect();
    let ticker: StringArray = rows.iter().map(|r| Some(r.ticker.as_str())).collect();
    let title_of_class: StringArray = rows
        .iter()
        .map(|r| Some(r.title_of_class.as_str()))
        .collect();
    let value_usd: Int64Array = rows.iter().map(|r| Some(r.value_usd)).collect();
    let shares: Int64Array = rows.iter().map(|r| Some(r.shares)).collect();
    let share_type: StringArray = rows.iter().map(|r| Some(r.share_type.as_str())).collect();
    let put_call: StringArray = rows.iter().map(|r| Some(r.put_call.as_str())).collect();
    let discretion: StringArray = rows.iter().map(|r| Some(r.discretion.as_str())).collect();

    RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(report_period),
            Arc::new(filed_date),
            Arc::new(accession),
            Arc::new(manager_cik),
            Arc::new(manager_name),
            Arc::new(issuer_name),
            Arc::new(cusip),
            Arc::new(ticker),
            Arc::new(title_of_class),
            Arc::new(value_usd),
            Arc::new(shares),
            Arc::new(share_type),
            Arc::new(put_call),
            Arc::new(discretion),
        ],
    )
    .map_err(Error::Arrow)
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

fn column_as<'a, A: Array + 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a A> {
    let idx = batch
        .schema()
        .index_of(name)
        .map_err(|_| Error::Parquet(format!("missing column: {name}")))?;
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<A>()
        .ok_or_else(|| Error::Parquet(format!("{name} column type mismatch")))
}

#[inline]
fn require_non_null(col: &dyn Array, field: &str, i: usize) -> Result<()> {
    if col.is_null(i) {
        Err(Error::Parquet(format!("null {field} at row {i}")))
    } else {
        Ok(())
    }
}

/// Parse a parquet file (in-memory bytes) into [`Holding`] records.
pub fn read_holdings(bytes: &[u8]) -> Result<Vec<Holding>> {
    let owned: bytes::Bytes = bytes::Bytes::copy_from_slice(bytes);
    let reader = ParquetRecordBatchReaderBuilder::try_new(owned)?.build()?;

    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch?;
        let report_period = column_as::<Int32Array>(&batch, "report_period")?;
        let filed_date = column_as::<Int32Array>(&batch, "filed_date")?;
        let accession = column_as::<StringArray>(&batch, "accession")?;
        let manager_cik = column_as::<UInt32Array>(&batch, "manager_cik")?;
        let manager_name = column_as::<StringArray>(&batch, "manager_name")?;
        let issuer_name = column_as::<StringArray>(&batch, "issuer_name")?;
        let cusip = column_as::<StringArray>(&batch, "cusip")?;
        let ticker = column_as::<StringArray>(&batch, "ticker")?;
        let title_of_class = column_as::<StringArray>(&batch, "title_of_class")?;
        let value_usd = column_as::<Int64Array>(&batch, "value_usd")?;
        let shares = column_as::<Int64Array>(&batch, "shares")?;
        let share_type = column_as::<StringArray>(&batch, "share_type")?;
        let put_call = column_as::<StringArray>(&batch, "put_call")?;
        let discretion = column_as::<StringArray>(&batch, "discretion")?;

        for i in 0..batch.num_rows() {
            require_non_null(report_period, "report_period", i)?;
            require_non_null(filed_date, "filed_date", i)?;
            require_non_null(accession, "accession", i)?;
            require_non_null(manager_cik, "manager_cik", i)?;
            require_non_null(cusip, "cusip", i)?;

            rows.push(Holding {
                report_period: report_period.value(i),
                filed_date: filed_date.value(i),
                accession: accession.value(i).to_owned(),
                manager_cik: manager_cik.value(i),
                manager_name: manager_name.value(i).to_owned(),
                issuer_name: issuer_name.value(i).to_owned(),
                cusip: cusip.value(i).to_owned(),
                ticker: ticker.value(i).to_owned(),
                title_of_class: title_of_class.value(i).to_owned(),
                value_usd: value_usd.value(i),
                shares: shares.value(i),
                share_type: share_type.value(i).to_owned(),
                put_call: put_call.value(i).to_owned(),
                discretion: discretion.value(i).to_owned(),
            });
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Holding {
        Holding {
            report_period: 20240331,
            filed_date: 20240514,
            accession: "0001067983-24-000011".into(),
            manager_cik: 1067983,
            manager_name: "BERKSHIRE HATHAWAY INC".into(),
            issuer_name: "APPLE INC".into(),
            cusip: "037833100".into(),
            ticker: String::new(),
            title_of_class: "COM".into(),
            value_usd: 135_360_000_000,
            shares: 789_368_450,
            share_type: "SH".into(),
            put_call: String::new(),
            discretion: "SOLE".into(),
        }
    }

    #[test]
    fn round_trips_rows() {
        let dir = std::env::temp_dir().join("fundskit_pq_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fund13f-2024Q1.parquet");
        let rows = vec![sample()];
        write_holdings(&path, &rows).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let back = read_holdings(&bytes).unwrap();
        assert_eq!(back, rows);
    }

    #[test]
    fn rejects_null_in_non_nullable_cusip() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("report_period", DataType::Int32, false),
            Field::new("filed_date", DataType::Int32, false),
            Field::new("accession", DataType::Utf8, false),
            Field::new("manager_cik", DataType::UInt32, false),
            Field::new("manager_name", DataType::Utf8, false),
            Field::new("issuer_name", DataType::Utf8, false),
            Field::new("cusip", DataType::Utf8, true), // nullable — the bad case
            Field::new("ticker", DataType::Utf8, false),
            Field::new("title_of_class", DataType::Utf8, false),
            Field::new("value_usd", DataType::Int64, false),
            Field::new("shares", DataType::Int64, false),
            Field::new("share_type", DataType::Utf8, false),
            Field::new("put_call", DataType::Utf8, false),
            Field::new("discretion", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![20240331])),
                Arc::new(Int32Array::from(vec![20240514])),
                Arc::new(StringArray::from(vec!["a"])),
                Arc::new(UInt32Array::from(vec![1u32])),
                Arc::new(StringArray::from(vec!["M"])),
                Arc::new(StringArray::from(vec!["X"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(StringArray::from(vec![""])),
                Arc::new(StringArray::from(vec!["COM"])),
                Arc::new(Int64Array::from(vec![1i64])),
                Arc::new(Int64Array::from(vec![1i64])),
                Arc::new(StringArray::from(vec!["SH"])),
                Arc::new(StringArray::from(vec![""])),
                Arc::new(StringArray::from(vec!["SOLE"])),
            ],
        )
        .unwrap();
        let mut buf = Vec::new();
        {
            let mut w = ArrowWriter::try_new(&mut buf, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }
        let err = read_holdings(&buf).unwrap_err().to_string();
        assert!(err.contains("null cusip"), "got: {err}");
    }
}

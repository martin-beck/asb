//! Bounded, offline long-form export of the same validated report inputs.
use super::*;
use parquet::data_type::{ByteArray, ByteArrayType, DoubleType};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use std::sync::Arc;

pub(super) const GROUP_ROWS: usize = 128;
pub(super) const MAX_ROW_BYTES: usize = 256 * 1024;
const TEXT_COLUMNS: [&str; 12] = [
    "row_id",
    "agent",
    "provider",
    "model",
    "workload",
    "trial",
    "family",
    "name",
    "unit",
    "scope",
    "status",
    "details_json",
];

pub(super) struct Record {
    pub(super) text: [String; 12],
    pub(super) value: Option<f64>,
    pub(super) timestamp: Option<f64>,
    pub(super) exact: Option<String>,
}
pub(super) struct Export {
    writer: SerializedFileWriter<fs::File>,
    rows: Vec<Record>,
}
pub(super) fn failure(error: impl std::fmt::Display) -> Error {
    Error::Io(format!("cannot publish columnar observations: {error}"))
}
impl Export {
    pub(super) fn new(path: &Path) -> Result<Self, Error> {
        let mut schema = String::from("message asb_observations {");
        for name in TEXT_COLUMNS {
            schema.push_str(&format!(" REQUIRED BINARY {name} (UTF8);"));
        }
        schema.push_str(" OPTIONAL DOUBLE value; OPTIONAL DOUBLE timestamp_seconds; OPTIONAL BINARY exact_integer (UTF8); }");
        let properties = WriterProperties::builder().set_compression(parquet::basic::Compression::ZSTD(Default::default())).set_key_value_metadata(Some(vec![
            parquet::file::metadata::KeyValue::new("asb.schema_version".into(),Some("1".into())),
            parquet::file::metadata::KeyValue::new("asb.semantics".into(),Some("Long-form retained measurements and regenerated diagnostics. Null is unavailable, never an invented zero. Exact integers are decimal strings. Timestamp seconds are collector-relative; inspect details_json for origin and limitations. No additional collection or inference.".into())),
        ])).build();
        Ok(Self {
            writer: SerializedFileWriter::new(
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map_err(failure)?,
                Arc::new(parse_message_type(&schema).map_err(failure)?),
                Arc::new(properties),
            )
            .map_err(failure)?,
            rows: Vec::with_capacity(GROUP_ROWS),
        })
    }
    pub(super) fn push(&mut self, row: Record) -> Result<(), Error> {
        if row.text.iter().map(String::len).sum::<usize>()
            + row.exact.as_ref().map_or(0, String::len)
            > MAX_ROW_BYTES
        {
            return Err(failure(
                "an observation exceeds the 256 KiB row bound; original inputs and previous report are preserved",
            ));
        }
        if row.value.is_some_and(|v| !v.is_finite())
            || row.timestamp.is_some_and(|v| !v.is_finite() || v < 0.0)
        {
            return Err(failure("nonfinite observation or invalid timestamp"));
        }
        self.rows.push(row);
        if self.rows.len() == GROUP_ROWS {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Error> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let mut group = self.writer.next_row_group().map_err(failure)?;
        for index in 0..TEXT_COLUMNS.len() {
            let mut column = group
                .next_column()
                .map_err(failure)?
                .ok_or_else(|| failure("missing text column"))?;
            let values = self
                .rows
                .iter()
                .map(|r| ByteArray::from(r.text[index].as_bytes()))
                .collect::<Vec<_>>();
            column
                .typed::<ByteArrayType>()
                .write_batch(&values, None, None)
                .map_err(failure)?;
            column.close().map_err(failure)?;
        }
        for timestamp in [false, true] {
            let optional = self
                .rows
                .iter()
                .map(|r| if timestamp { r.timestamp } else { r.value })
                .collect::<Vec<_>>();
            let values = optional.iter().filter_map(|v| *v).collect::<Vec<_>>();
            let levels = optional
                .iter()
                .map(|v| i16::from(v.is_some()))
                .collect::<Vec<_>>();
            let mut column = group
                .next_column()
                .map_err(failure)?
                .ok_or_else(|| failure("missing numeric column"))?;
            column
                .typed::<DoubleType>()
                .write_batch(&values, Some(&levels), None)
                .map_err(failure)?;
            column.close().map_err(failure)?;
        }
        let mut column = group
            .next_column()
            .map_err(failure)?
            .ok_or_else(|| failure("missing exact-integer column"))?;
        let values = self
            .rows
            .iter()
            .filter_map(|r| r.exact.as_deref())
            .map(ByteArray::from)
            .collect::<Vec<_>>();
        let levels = self
            .rows
            .iter()
            .map(|r| i16::from(r.exact.is_some()))
            .collect::<Vec<_>>();
        column
            .typed::<ByteArrayType>()
            .write_batch(&values, Some(&levels), None)
            .map_err(failure)?;
        column.close().map_err(failure)?;
        group.close().map_err(failure)?;
        self.rows.clear();
        Ok(())
    }
    pub(super) fn close(mut self) -> Result<(), Error> {
        self.flush()?;
        self.writer.close().map_err(failure)?;
        Ok(())
    }
}

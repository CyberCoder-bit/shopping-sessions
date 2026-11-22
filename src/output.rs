//! Parquet writers for the two output tables, with the exact schema the checker wants.
//!
//! Fill a `UserDaily` / `CampaignDaily` (column vectors, one entry per row) and call
//! `write_*` with a file name; several part files per table are fine (row order and file
//! count do not matter). `day` is days since 1970-01-01 (Parquet DATE).

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Default)]
pub struct UserDaily {
    pub user_id: Vec<ByteArray>,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue_cents: Vec<i64>,
    /// `None` = null (no purchases that person-day).
    pub top_category: Vec<Option<ByteArray>>,
}

#[derive(Default)]
pub struct CampaignDaily {
    pub campaign_id: Vec<ByteArray>,
    pub channel: Vec<ByteArray>,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub purchasing_sessions: Vec<i64>,
    pub revenue_cents: Vec<i64>,
    pub assisted_sessions: Vec<i64>,
}

const USER_DAILY_SCHEMA: &str = "message user_daily {
    REQUIRED BYTE_ARRAY user_id (UTF8);
    REQUIRED INT32 day (DATE);
    REQUIRED INT64 sessions;
    REQUIRED INT64 events;
    REQUIRED INT64 revenue_cents;
    OPTIONAL BYTE_ARRAY top_category (UTF8);
}";

const CAMPAIGN_DAILY_SCHEMA: &str = "message campaign_daily {
    REQUIRED BYTE_ARRAY campaign_id (UTF8);
    REQUIRED BYTE_ARRAY channel (UTF8);
    REQUIRED INT32 day (DATE);
    REQUIRED INT64 sessions;
    REQUIRED INT64 purchasing_sessions;
    REQUIRED INT64 revenue_cents;
    REQUIRED INT64 assisted_sessions;
}";

enum Col<'a> {
    Str(&'a [ByteArray]),
    OptStr(&'a [Option<ByteArray>]),
    I32(&'a [i32]),
    I64(&'a [i64]),
}

impl Col<'_> {
    fn len(&self) -> usize {
        match self {
            Col::Str(v) => v.len(),
            Col::OptStr(v) => v.len(),
            Col::I32(v) => v.len(),
            Col::I64(v) => v.len(),
        }
    }
}

/// Writes `dir/name` (creating `dir`): one row group, uncompressed, no dictionary, no
/// statistics. The output is small, so encoding speed beats file size.
fn write_table(dir: &Path, name: &str, schema: &str, cols: &[Col]) -> Result<()> {
    let rows = cols[0].len();
    assert!(cols.iter().all(|c| c.len() == rows), "ragged columns");
    fs::create_dir_all(dir)?;
    let file = BufWriter::with_capacity(1 << 20, File::create(dir.join(name))?);
    let schema = Arc::new(parse_message_type(schema)?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .build(),
    );
    let mut writer = SerializedFileWriter::new(file, schema, props)?;
    if rows > 0 {
        let mut group = writer.next_row_group()?;
        let mut at = 0;
        while let Some(mut column) = group.next_column()? {
            match &cols[at] {
                Col::Str(v) => {
                    column.typed::<ByteArrayType>().write_batch(v, None, None)?;
                }
                Col::OptStr(v) => {
                    let values: Vec<ByteArray> = v.iter().flatten().cloned().collect();
                    let levels: Vec<i16> = v.iter().map(|s| s.is_some() as i16).collect();
                    column.typed::<ByteArrayType>().write_batch(&values, Some(&levels), None)?;
                }
                Col::I32(v) => {
                    column.typed::<Int32Type>().write_batch(v, None, None)?;
                }
                Col::I64(v) => {
                    column.typed::<Int64Type>().write_batch(v, None, None)?;
                }
            }
            column.close()?;
            at += 1;
        }
        group.close()?;
    }
    let buffered = writer.into_inner()?;
    buffered.into_inner().map_err(|e| e.into_error())?;
    Ok(())
}

pub fn write_user_daily(out_dir: &Path, name: &str, t: &UserDaily) -> Result<()> {
    write_table(
        &out_dir.join("user_daily"),
        name,
        USER_DAILY_SCHEMA,
        &[
            Col::Str(&t.user_id),
            Col::I32(&t.day),
            Col::I64(&t.sessions),
            Col::I64(&t.events),
            Col::I64(&t.revenue_cents),
            Col::OptStr(&t.top_category),
        ],
    )
}

pub fn write_campaign_daily(out_dir: &Path, name: &str, t: &CampaignDaily) -> Result<()> {
    write_table(
        &out_dir.join("campaign_daily"),
        name,
        CAMPAIGN_DAILY_SCHEMA,
        &[
            Col::Str(&t.campaign_id),
            Col::Str(&t.channel),
            Col::I32(&t.day),
            Col::I64(&t.sessions),
            Col::I64(&t.purchasing_sessions),
            Col::I64(&t.revenue_cents),
            Col::I64(&t.assisted_sessions),
        ],
    )
}

// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The sidecar: a [`Row`] a frame, as Parquet, one column a field.
//!
//! Vectors are flattened into a column each (`position_x`, `position_y`,
//! …) so that a frame is one flat record every tool reads. A value the
//! recording has not got is null, never a made-up number.

use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, RecordBatch, StringArray, UInt32Array,
    UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};

use crate::Row;

type F64 = (&'static str, fn(&Row) -> Option<f64>, fn(&mut Row, f64));
type U64 = (&'static str, fn(&Row) -> Option<u64>, fn(&mut Row, u64));

fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

macro_rules! f64s {
    ($( $name:literal => $($field:ident).+ $([$i:literal])? ),* $(,)?) => {
        &[ $( (
            $name,
            |r: &Row| finite(r.$($field).+ $([$i])?),
            |r: &mut Row, v: f64| r.$($field).+ $([$i])? = v,
        ) ),* ]
    };
}

const F64S: &[F64] = f64s![
    "wall" => wall, "instant" => instant, "rate" => rate, "offset" => offset,
    "scale" => scale,
    "render_origin_x" => render_origin[0], "render_origin_y" => render_origin[1],
    "render_origin_z" => render_origin[2], "origin_step" => origin_step,
    "position_x" => position[0], "position_y" => position[1], "position_z" => position[2],
    "direction_x" => direction[0], "direction_y" => direction[1], "direction_z" => direction[2],
    "up_x" => up[0], "up_y" => up[1], "up_z" => up[2], "fovy" => fovy,
    "lon" => lon, "lat" => lat, "altitude" => altitude, "heading" => heading, "pitch" => pitch,
    "white_balance_r" => white_balance[0], "white_balance_g" => white_balance[1],
    "white_balance_b" => white_balance[2],
    "ambient_r" => ambient[0], "ambient_g" => ambient[1], "ambient_b" => ambient[2],
    "illuminance_r" => illuminance[0], "illuminance_g" => illuminance[1],
    "illuminance_b" => illuminance[2],
    "to_sun_x" => to_sun[0], "to_sun_y" => to_sun[1], "to_sun_z" => to_sun[2],
    "luma" => luma, "sky_luma" => sky_luma, "ground_luma" => ground_luma,
    "delta" => delta, "sky_delta" => sky_delta, "ground_delta" => ground_delta,
];

const U64S: &[U64] = &[
    ("frame", |r| Some(r.frame), |r, v| r.frame = v),
    ("light_updates", |r| r.light_updates, |r, v| r.light_updates = Some(v)),
    ("view_updates", |r| r.view_updates, |r, v| r.view_updates = Some(v)),
    ("sky_pixels", |r| Some(r.sky_pixels), |r, v| r.sky_pixels = v),
    ("ground_pixels", |r| Some(r.ground_pixels), |r, v| r.ground_pixels = v),
    ("stars", |r| Some(r.stars), |r, v| r.stars = v),
    ("black", |r| Some(r.black), |r, v| r.black = v),
    ("black_ground", |r| Some(r.black_ground), |r, v| r.black_ground = v),
];

/// Writes `rows` to `path`.
pub fn write(path: &std::path::Path, rows: &[Row]) -> Result<(), String> {
    let mut fields = Vec::new();
    let mut columns: Vec<ArrayRef> = Vec::new();
    for (name, get, _) in U64S {
        fields.push(Field::new(*name, DataType::UInt64, true));
        columns.push(Arc::new(rows.iter().map(get).collect::<UInt64Array>()));
    }
    for (name, get) in [
        ("viewport_width", (|r: &Row| r.viewport[0]) as fn(&Row) -> u32),
        ("viewport_height", |r: &Row| r.viewport[1]),
        ("picture_width", |r: &Row| r.picture[0]),
        ("picture_height", |r: &Row| r.picture[1]),
    ] {
        fields.push(Field::new(name, DataType::UInt32, false));
        columns.push(Arc::new(rows.iter().map(get).collect::<UInt32Array>()));
    }
    for (name, get, _) in F64S {
        fields.push(Field::new(*name, DataType::Float64, true));
        columns.push(Arc::new(rows.iter().map(get).collect::<Float64Array>()));
    }
    fields.push(Field::new("exposure", DataType::Float64, true));
    columns.push(Arc::new(
        rows.iter().map(|r| r.exposure).collect::<Float64Array>(),
    ));
    fields.push(Field::new("pictured", DataType::Boolean, false));
    columns.push(Arc::new(
        rows.iter().map(|r| Some(r.pictured)).collect::<BooleanArray>(),
    ));
    fields.push(Field::new("input", DataType::Utf8, false));
    columns.push(Arc::new(
        rows.iter().map(|r| Some(r.input.as_str())).collect::<StringArray>(),
    ));
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(Arc::clone(&schema), columns).map_err(|e| e.to_string())?;
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::SNAPPY)
        .build();
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, schema, Some(props))
        .map_err(|e| e.to_string())?;
    writer.write(&batch).map_err(|e| e.to_string())?;
    writer.close().map_err(|e| e.to_string())?;
    Ok(())
}

/// Reads the rows of the sidecar at `path` back.
pub fn read(path: &std::path::Path) -> Result<Vec<Row>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| e.to_string())?;
        let first = rows.len();
        rows.extend((0..batch.num_rows()).map(|_| Row {
            exposure: None,
            ..Row::default()
        }));
        let rows = &mut rows[first..];
        let column = |name: &str| {
            batch
                .column_by_name(name)
                .cloned()
                .ok_or_else(|| format!("the sidecar has no column {name}"))
        };
        for (name, _, set) in F64S {
            let c = column(name)?;
            let c = c
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| format!("{name} is not f64"))?;
            for (i, row) in rows.iter_mut().enumerate() {
                set(row, if c.is_null(i) { f64::NAN } else { c.value(i) });
            }
        }
        for (name, _, set) in U64S {
            let c = column(name)?;
            let c = c
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| format!("{name} is not u64"))?;
            for (i, row) in rows.iter_mut().enumerate() {
                if !c.is_null(i) {
                    set(row, c.value(i));
                }
            }
        }
        for (name, at, picture) in [
            ("viewport_width", 0, false),
            ("viewport_height", 1, false),
            ("picture_width", 0, true),
            ("picture_height", 1, true),
        ] {
            let c = column(name)?;
            let c = c
                .as_any()
                .downcast_ref::<UInt32Array>()
                .ok_or_else(|| format!("{name} is not u32"))?;
            for (i, row) in rows.iter_mut().enumerate() {
                let slot = if picture {
                    &mut row.picture
                } else {
                    &mut row.viewport
                };
                slot[at] = c.value(i);
            }
        }
        let exposure = column("exposure")?;
        let exposure = exposure
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("exposure is not f64")?;
        let pictured = column("pictured")?;
        let pictured = pictured
            .as_any()
            .downcast_ref::<BooleanArray>()
            .ok_or("pictured is not a boolean")?;
        let input = column("input")?;
        let input = input
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("input is not text")?;
        for (i, row) in rows.iter_mut().enumerate() {
            row.exposure = (!exposure.is_null(i)).then(|| exposure.value(i));
            row.pictured = pictured.value(i);
            row.input = input.value(i).to_owned();
        }
    }
    Ok(rows)
}

// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Outcomes as Parquet tables (spec §9.3): one row per run and phase.
//! Never CSV.

use std::sync::Arc;

use arrow_array::{
    ArrayRef, BooleanArray, Float64Array, RecordBatch, StringArray, UInt32Array, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};

use crate::objective::Outcome;

/// Writes `runs` to `path`: a row per phase of each run, the run's totals
/// repeated on each of its rows.
pub fn write(path: &std::path::Path, runs: &[Outcome]) -> Result<(), String> {
    let rows: Vec<(&Outcome, usize)> = runs
        .iter()
        .flat_map(|o| (0..o.phases.len().max(1)).map(move |k| (o, k)))
        .collect();
    let s = |f: &dyn Fn(&Outcome, usize) -> String| -> ArrayRef {
        Arc::new(StringArray::from(
            rows.iter().map(|(o, k)| f(o, *k)).collect::<Vec<_>>(),
        ))
    };
    let u = |f: &dyn Fn(&Outcome, usize) -> u64| -> ArrayRef {
        Arc::new(UInt64Array::from(
            rows.iter().map(|(o, k)| f(o, *k)).collect::<Vec<_>>(),
        ))
    };
    let x = |f: &dyn Fn(&Outcome, usize) -> f64| -> ArrayRef {
        Arc::new(Float64Array::from(
            rows.iter().map(|(o, k)| f(o, *k)).collect::<Vec<_>>(),
        ))
    };
    let phase = |o: &Outcome, k: usize| o.phases.get(k).cloned().unwrap_or_default();
    let columns: Vec<(&str, ArrayRef)> = vec![
        ("scenario", s(&|o, _| o.scenario.to_owned())),
        ("device", s(&|o, _| o.device.to_owned())),
        ("network", s(&|o, _| o.network.to_owned())),
        ("spec", s(&|o, _| o.spec.to_owned())),
        ("query", s(&|o, _| o.query.clone())),
        (
            "catalogue",
            Arc::new(UInt32Array::from(
                rows.iter().map(|(o, _)| o.catalogue).collect::<Vec<_>>(),
            )),
        ),
        ("phase", s(&|o, k| phase(o, k).phase.to_owned())),
        ("phase_hole_tile_steps", u(&|o, k| phase(o, k).hole_tile_steps)),
        ("phase_holes_max", u(&|o, k| u64::from(phase(o, k).holes_max))),
        (
            "phase_deficit_level_seconds",
            x(&|o, k| phase(o, k).deficit_level_seconds),
        ),
        ("phase_bytes_max", u(&|o, k| phase(o, k).bytes_max)),
        ("phase_starts", u(&|o, k| phase(o, k).starts)),
        ("phase_wasted_starts", u(&|o, k| phase(o, k).wasted_starts)),
        ("steps", u(&|o, _| o.steps)),
        ("deficit_level_seconds", x(&|o, _| o.deficit_level_seconds)),
        ("queue_seconds", x(&|o, _| o.queue_seconds)),
        ("hole_tile_steps", u(&|o, _| o.hole_tile_steps)),
        (
            "settle_seconds",
            Arc::new(Float64Array::from(
                rows.iter().map(|(o, _)| o.settle_seconds).collect::<Vec<_>>(),
            )),
        ),
        ("thrash", u(&|o, _| o.thrash)),
        ("j", x(&|o, _| o.j)),
        (
            "broke",
            Arc::new(BooleanArray::from(
                rows.iter().map(|(o, _)| o.broke.is_some()).collect::<Vec<_>>(),
            )),
        ),
    ];
    let fields: Vec<Field> = columns
        .iter()
        .map(|(n, a)| Field::new(*n, a.data_type().clone(), *a.data_type() == DataType::Float64))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        columns.into_iter().map(|(_, a)| a).collect(),
    )
    .map_err(|e| e.to_string())?;
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::SNAPPY)
        .build();
    let mut w = parquet::arrow::ArrowWriter::try_new(file, schema, Some(props))
        .map_err(|e| e.to_string())?;
    w.write(&batch).map_err(|e| e.to_string())?;
    w.close().map_err(|e| e.to_string())?;
    Ok(())
}

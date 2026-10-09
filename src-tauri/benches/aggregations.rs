//! Microbenchmark reproductible sans réseau, disque ni désérialisation dans la mesure.
use chrono::{Duration, Local, TimeZone};
use std::{hint::black_box, time::Instant};
#[allow(dead_code)]
#[path = "../src/litellm/domain.rs"]
mod domain;

fn main() {
    let start = Local
        .with_ymd_and_hms(2026, 10, 8, 12, 0, 0)
        .single()
        .unwrap();
    for size in [1_000, 10_000, 100_000] {
        let rows: Vec<domain::LogRow> = (0..size).map(|i| {
            serde_json::from_value(serde_json::json!({
                "model": format!("provider/model-{}", i % 20), "spend": 0.01,
                "total_tokens": 100, "prompt_tokens": 80, "completion_tokens": 20,
                "status": "success", "cache_hit": i % 4 == 0,
                "startTime": start.to_rfc3339(),
                "endTime": (start + Duration::milliseconds(1 + (i * 7919 % 10_000) as i64)).to_rfc3339(),
                "completionStartTime": (start + Duration::milliseconds(1 + (i * 97 % 1000) as i64)).to_rfc3339()
            })).unwrap()
        }).collect();
        let mut samples = Vec::new();
        for _ in 0..31 {
            let before = Instant::now();
            black_box(domain::aggregate_activity(
                black_box(&rows),
                start.date_naive(),
                &Local,
            ));
            samples.push(before.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(f64::total_cmp);
        println!("{size} rows: median {:.3} ms (31 samples)", samples[15]);
    }
}

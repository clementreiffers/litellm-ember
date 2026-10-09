//! Calculs déterministes : aucune entrée/sortie ni lecture de l'horloge.
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use serde::Deserialize;
use shared::{Activity, DayCost, HourBucket, ModelActivity, ModelCost, ModelUsage, TopRequest};
use std::collections::HashMap;

pub(super) fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// Un point par jour de `start` à `today` (les jours sans dépense valent 0), au plus les 14 derniers.
pub(super) fn aggregate_days(
    summary: &[DaySummary],
    start: NaiveDate,
    today: NaiveDate,
) -> Vec<DayCost> {
    let mut by_day: HashMap<NaiveDate, f64> = HashMap::new();
    for d in summary {
        if let Some(date) = d.start_time.as_deref().and_then(parse_date) {
            *by_day.entry(date).or_default() += d.spend;
        }
    }
    let first = start.max(today - Duration::days(13));
    (0..=(today - first).num_days())
        .map(|i| {
            let date = first + Duration::days(i);
            DayCost {
                date: date.to_string(),
                spend: by_day.get(&date).copied().unwrap_or(0.0),
            }
        })
        .collect()
}

/// Extrapole la dépense de la période au rythme moyen observé depuis son début.
#[cfg(test)]
pub(super) fn project(
    spend: f64,
    start: NaiveDate,
    reset: NaiveDate,
    now: DateTime<Utc>,
) -> Option<f64> {
    project_instants(
        spend,
        start.and_hms_opt(0, 0, 0)?.and_utc(),
        reset.and_hms_opt(0, 0, 0)?.and_utc(),
        now,
    )
}

pub struct KeyData {
    pub period_spend: f64,
    pub max_budget: Option<f64>,
    pub budget_reset_at: Option<String>,
    pub period_start: Option<String>,
}

/// Début de la période de budget : date de reset moins la durée du budget (`7d`, `24h`, `2w`, `1mo`).
pub(super) fn parse_instant(value: &str) -> Option<DateTime<Utc>> {
    value
        .parse()
        .ok()
        .or_else(|| parse_date(value)?.and_hms_opt(0, 0, 0).map(|d| d.and_utc()))
}
pub(super) fn period_start(info: &KeyInfoInner) -> Option<String> {
    let reset = parse_instant(info.budget_reset_at.as_deref()?)?;
    let duration = parse_duration(info.budget_duration.as_deref().unwrap_or("7d"))?;
    Some(reset.checked_sub_signed(duration)?.to_rfc3339())
}
pub(super) fn parse_duration(value: &str) -> Option<Duration> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let n: i64 = value[..split].parse().ok()?;
    if n <= 0 {
        return None;
    }
    let factor = match &value[split..] {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" | "" => 86400,
        "w" => 7 * 86400,
        "mo" => 30 * 86400,
        _ => return None,
    };
    Duration::try_seconds(n.checked_mul(factor)?)
}
pub(super) fn project_instants(
    spend: f64,
    start: DateTime<Utc>,
    reset: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let total = (reset - start).num_seconds() as f64;
    let elapsed = (now - start).num_seconds() as f64;
    let projection = spend / elapsed * total;
    (total > 0.0 && elapsed >= 21600.0 && now <= reset && projection.is_finite())
        .then_some(projection)
}

#[derive(Deserialize)]
pub(super) struct KeyInfo {
    pub(super) info: KeyInfoInner,
}

#[derive(Deserialize)]
pub(super) struct KeyInfoInner {
    pub(super) spend: f64,
    pub(super) max_budget: Option<f64>,
    pub(super) budget_reset_at: Option<String>,
    pub(super) budget_duration: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct DaySummary {
    #[serde(default)]
    pub(super) models: HashMap<String, f64>,
    #[serde(default)]
    pub(super) spend: f64,
    #[serde(default, rename = "startTime")]
    pub(super) start_time: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct LogRow {
    #[serde(default)]
    pub(super) model: String,
    #[serde(default)]
    pub(super) spend: f64,
    #[serde(default, rename = "prompt_tokens")]
    pub(super) prompt: u64,
    #[serde(default, rename = "completion_tokens")]
    pub(super) completion: u64,
    #[serde(default, rename = "total_tokens")]
    pub(super) total: u64,
    #[serde(rename = "startTime")]
    pub(super) start_time: Option<DateTime<Utc>>,
    #[serde(default, rename = "endTime")]
    pub(super) end_time: Option<DateTime<Utc>>,
    #[serde(default, rename = "completionStartTime")]
    pub(super) completion_start: Option<DateTime<Utc>>,
    /// `"success"` ou `"failure"`.
    #[serde(default)]
    pub(super) status: Option<String>,
    /// Booléen ou chaîne (`"True"`, `"False"`, `"None"`) selon la version de LiteLLM.
    #[serde(default)]
    pub(super) cache_hit: Option<serde_json::Value>,
}

impl LogRow {
    pub(super) fn failed(&self) -> bool {
        self.status.as_deref() == Some("failure")
    }

    /// `Some(true)` si servie par le cache, `Some(false)` si le cache a été consulté sans succès, `None` s'il n'est pas utilisé.
    pub(super) fn cache(&self) -> Option<bool> {
        use serde_json::Value;
        match self.cache_hit.as_ref()? {
            Value::Bool(b) => Some(*b),
            Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
            Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
            _ => None,
        }
    }

    pub(super) fn latency_ms(&self) -> Option<f64> {
        let ms = (self.end_time? - self.start_time?).num_milliseconds() as f64;
        (ms > 0.0).then_some(ms)
    }

    pub(super) fn ttft_ms(&self) -> Option<f64> {
        let ms = (self.completion_start? - self.start_time?).num_milliseconds() as f64;
        (ms > 0.0).then_some(ms)
    }
}

/// `openai/gpt-6-sol` et `gpt-6-sol` sont le même modèle ; `bedrock/invoke/x` → `x`.
/// Renvoie `None` pour un nom vide.
pub fn normalize_model(name: &str) -> Option<&str> {
    let short = name.rsplit('/').next().unwrap_or(name).trim();
    (!short.is_empty()).then_some(short)
}

pub(super) fn aggregate_totals(days: &[DaySummary]) -> Vec<ModelCost> {
    let mut acc: HashMap<&str, f64> = HashMap::new();
    for (name, spend) in days.iter().flat_map(|d| d.models.iter()) {
        if let Some(model) = normalize_model(name) {
            *acc.entry(model).or_default() += spend;
        }
    }
    let mut out: Vec<_> = acc
        .into_iter()
        .map(|(model, spend)| ModelCost {
            model: model.to_owned(),
            spend,
        })
        .collect();
    out.sort_by(|a, b| b.spend.total_cmp(&a.spend).then(a.model.cmp(&b.model)));
    out
}

pub(super) fn aggregate_today(
    rows: &[LogRow],
    today: NaiveDate,
    timezone: &impl chrono::TimeZone,
) -> Vec<ModelUsage> {
    let mut acc: HashMap<&str, ModelUsage> = HashMap::new();
    for row in rows {
        let Some(start) = row.start_time else {
            continue;
        };
        if start.with_timezone(timezone).date_naive() != today {
            continue;
        }
        let Some(model) = normalize_model(&row.model) else {
            continue;
        };
        let u = acc.entry(model).or_insert_with(|| ModelUsage {
            model: model.to_owned(),
            ..Default::default()
        });
        u.spend += row.spend;
        u.requests += 1;
        u.prompt_tokens += row.prompt;
        u.completion_tokens += row.completion;
        u.total_tokens += row.total;
    }
    let mut out: Vec<_> = acc.into_values().collect();
    out.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then(a.model.cmp(&b.model))
    });
    out
}

/// Percentile par rang le plus proche ; `None` si la liste est vide.
pub(super) fn percentile_sorted(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let idx = ((p / 100.0) * (values.len() - 1) as f64).round() as usize;
    Some(values[idx])
}

pub(super) fn aggregate_activity(
    rows: &[LogRow],
    today: NaiveDate,
    timezone: &impl chrono::TimeZone,
) -> Activity {
    let mut hourly = vec![HourBucket::default(); 24];
    let mut request_count = 0_u64;
    let mut top: Option<&LogRow> = None;

    #[derive(Default)]
    struct Acc {
        requests: u64,
        errors: u64,
        spend: f64,
        tokens: u64,
        latencies: Vec<f64>,
    }
    let mut per_model: HashMap<&str, Acc> = HashMap::new();
    let (mut errors, mut spend, mut tokens) = (0_u64, 0.0_f64, 0_u64);
    let (mut hits, mut tracked) = (0_u64, 0_u64);
    let (mut latencies, mut ttfts) = (Vec::new(), Vec::new());

    for r in rows {
        let Some(local) = r.start_time.map(|t| t.with_timezone(timezone)) else {
            continue;
        };
        if local.date_naive() != today {
            continue;
        }
        request_count += 1;
        if r.spend > 0.0 && top.is_none_or(|previous| r.spend.total_cmp(&previous.spend).is_ge()) {
            top = Some(r);
        }
        let failed = r.failed();
        errors += failed as u64;
        spend += r.spend;
        tokens += r.total;
        if let Some(hit) = r.cache() {
            tracked += 1;
            hits += hit as u64;
        }
        if !failed {
            latencies.extend(r.latency_ms());
            ttfts.extend(r.ttft_ms());
        }
        let bucket = &mut hourly[local.hour() as usize];
        bucket.requests += 1;
        bucket.spend += r.spend;
        if let Some(model) = normalize_model(&r.model) {
            let a = per_model.entry(model).or_default();
            a.requests += 1;
            a.errors += failed as u64;
            a.spend += r.spend;
            a.tokens += r.total;
            if !failed {
                a.latencies.extend(r.latency_ms());
            }
        }
    }

    let requests = request_count.max(1) as f64;
    latencies.sort_by(f64::total_cmp);
    ttfts.sort_by(f64::total_cmp);
    let mut per_model: Vec<ModelActivity> = per_model
        .into_iter()
        .map(|(model, mut a)| {
            let requests = a.requests.max(1) as f64;
            a.latencies.sort_by(f64::total_cmp);
            ModelActivity {
                model: model.to_owned(),
                requests: a.requests,
                errors: a.errors,
                spend: a.spend,
                avg_cost: a.spend / requests,
                avg_tokens: a.tokens as f64 / requests,
                latency_p50_ms: percentile_sorted(&a.latencies, 50.0),
                latency_p95_ms: percentile_sorted(&a.latencies, 95.0),
            }
        })
        .collect();
    per_model.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.model.cmp(&b.model)));

    let top_request = top.map(|r| TopRequest {
        model: normalize_model(&r.model).unwrap_or_default().to_owned(),
        spend: r.spend,
        total_tokens: r.total,
        time: r
            .start_time
            .map(|t| {
                t.with_timezone(timezone)
                    .naive_local()
                    .format("%H:%M")
                    .to_string()
            })
            .unwrap_or_default(),
    });

    Activity {
        requests: request_count,
        errors,
        avg_cost: spend / requests,
        avg_tokens: tokens as f64 / requests,
        cache_rate: (tracked > 0).then(|| hits as f64 / tracked as f64),
        latency_p50_ms: percentile_sorted(&latencies, 50.0),
        latency_p95_ms: percentile_sorted(&latencies, 95.0),
        ttft_p50_ms: percentile_sorted(&ttfts, 50.0),
        per_model,
        hourly,
        top_request,
    }
}

#[cfg(test)]
use chrono::DateTime;
use chrono::{Duration, Local, NaiveDate, Utc};
mod analytics;
mod costs;
mod daily;
mod domain;
pub use domain::KeyData;
use domain::*;
use serde::Deserialize;
use shared::{Activity, ModelCost, ModelUsage, WeekDetails};

#[derive(Debug, thiserror::Error)]
enum TransportError {
    #[error("Historique tronqué par LiteLLM ; impossible de calculer un total complet")]
    Truncated,
    #[error("{0}")]
    Request(String),
    #[error("HTTP {status} sur {path}")]
    Status {
        status: reqwest::StatusCode,
        path: String,
    },
    #[error("réponse invalide ({path}): {source}")]
    Decode {
        path: String,
        source: reqwest::Error,
    },
}
impl From<TransportError> for String {
    fn from(error: TransportError) -> Self {
        error.to_string()
    }
}

#[derive(Clone)]
pub struct Client {
    rows: std::sync::Arc<tokio::sync::Mutex<Option<RowsCache>>>,
    daily: std::sync::Arc<tokio::sync::Mutex<daily::DailyState>>,
    analytics: std::sync::Arc<tokio::sync::Mutex<analytics::Access>>,
    costs: std::sync::Arc<tokio::sync::Mutex<costs::CostState>>,
    summaries: std::sync::Arc<tokio::sync::Mutex<Option<SummaryCache>>>,
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

struct RowsCache {
    fetched: std::time::Instant,
    day: NaiveDate,
    rows: std::sync::Arc<Vec<LogRow>>,
}
struct SummaryCache {
    fetched: std::time::Instant,
    path: String,
    rows: std::sync::Arc<Vec<DaySummary>>,
}

impl Client {
    pub async fn fetch_model_prices(&self) -> Result<serde_json::Value, String> {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.get("/model/info"))
            .await
            .map_err(|_| "Tarifs indisponibles".to_owned())?
            .map_err(Into::into)
    }
    #[cfg(test)]
    pub fn new(base_url: &str, api_key: String) -> Result<Self, String> {
        Self::with_http(base_url, api_key, Self::http()?)
    }

    pub fn http() -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(describe)
    }

    pub fn with_http(
        base_url: &str,
        api_key: String,
        http: reqwest::Client,
    ) -> Result<Self, String> {
        if base_url.trim().is_empty() {
            return Err("Endpoint LiteLLM manquant".into());
        }
        Ok(Self {
            http,
            base_url: crate::settings::validate_url(base_url)?,
            api_key,
            rows: Default::default(),
            daily: Default::default(),
            analytics: Default::default(),
            costs: Default::default(),
            summaries: Default::default(),
        })
    }

    async fn summary(&self, path: String) -> Result<std::sync::Arc<Vec<DaySummary>>, String> {
        let mut cache = self.summaries.lock().await;
        if let Some(c) = cache
            .as_ref()
            .filter(|c| c.path == path && c.fetched.elapsed().as_secs() < 60)
        {
            return Ok(c.rows.clone());
        }
        let rows = std::sync::Arc::new(self.get::<Vec<DaySummary>>(&path).await?);
        *cache = Some(SummaryCache {
            fetched: std::time::Instant::now(),
            path,
            rows: rows.clone(),
        });
        Ok(rows)
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, TransportError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| TransportError::Request(describe(error)))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(TransportError::Status {
                status,
                path: path.to_owned(),
            });
        }
        if resp
            .headers()
            .get("x-litellm-spend-logs-truncated")
            .is_some_and(|value| value == "true")
        {
            return Err(TransportError::Truncated);
        }
        resp.json().await.map_err(|source| TransportError::Decode {
            path: path.to_owned(),
            source: source.without_url(),
        })
    }

    /// Étape 1 (≈0,5 s) : dépense de la période de budget en cours (`/key/info`).
    pub async fn fetch_key(&self) -> Result<KeyData, String> {
        let key: KeyInfo = self.get("/key/info").await?;
        Ok(KeyData {
            period_spend: key.info.spend,
            max_budget: key.info.max_budget,
            budget_reset_at: key.info.budget_reset_at.clone(),
            period_start: period_start(&key.info),
        })
    }

    /// Étape 2 (≈0,5 s) : coût par modèle depuis le début de la période (agrégat, sans tokens).
    pub async fn fetch_models(&self, period_start: Option<&str>) -> Result<Vec<ModelCost>, String> {
        let now = Utc::now().date_naive();
        let tomorrow = now + Duration::days(1);
        let start = period_start
            .and_then(parse_date)
            .map(|d| d.to_string())
            .unwrap_or_else(|| (now - Duration::days(7)).to_string());
        let summary = self
            .summary(format!(
                "/spend/logs?summarize=true&start_date={start}&end_date={tomorrow}"
            ))
            .await?;
        Ok(aggregate_totals(&summary))
    }

    /// Étape 3 (lente, 5 à 10 s : l'API renvoie les messages) : tokens et $ du jour par modèle.
    #[cfg(test)]
    pub async fn fetch_today(&self) -> Result<Vec<ModelUsage>, String> {
        self.fetch_today_snapshot().await.map(|(_, rows)| rows)
    }

    pub async fn fetch_today_snapshot(&self) -> Result<(NaiveDate, Vec<ModelUsage>), String> {
        let (today, rows) = self.fetch_today_rows().await?;
        Ok((today, aggregate_today(&rows, today, &Local)))
    }

    async fn fetch_today_rows(&self) -> Result<(NaiveDate, std::sync::Arc<Vec<LogRow>>), String> {
        let now = Local::now();
        let today = now.date_naive();
        let mut cache = self.rows.lock().await;
        if let Some(c) = cache
            .as_ref()
            .filter(|c| c.day == today && c.fetched.elapsed().as_secs() < 60)
        {
            return Ok((today, c.rows.clone()));
        }
        let start = today
            .and_hms_opt(0, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).earliest())
            .map(|d| d.with_timezone(&Utc).date_naive())
            .unwrap_or(now.with_timezone(&Utc).date_naive());
        let end = now.with_timezone(&Utc).date_naive() + Duration::days(1);
        let rows = std::sync::Arc::new(
            self.get::<Vec<LogRow>>(&format!(
                "/spend/logs?summarize=false&start_date={start}&end_date={end}"
            ))
            .await?,
        );
        *cache = Some(RowsCache {
            fetched: std::time::Instant::now(),
            day: today,
            rows: rows.clone(),
        });
        Ok((today, rows))
    }

    pub async fn fetch_activity(&self) -> Result<Activity, String> {
        let (today, rows) = self.fetch_today_rows().await?;
        Ok(aggregate_activity(&rows, today, &Local))
    }

    /// Agrégats légers : ne désérialise ni les modèles ni les messages.
    pub async fn fetch_history(
        &self,
        today: NaiveDate,
        start: NaiveDate,
    ) -> Result<[Option<f64>; crate::history::DAYS], String> {
        #[derive(Deserialize)]
        struct DailySpend {
            #[serde(rename = "startTime")]
            date: Option<String>,
            spend: f64,
        }
        let end = today + Duration::days(1);
        let rows: Vec<DailySpend> = self
            .get(&format!(
                "/spend/logs?summarize=true&start_date={start}&end_date={end}"
            ))
            .await?;
        crate::history::aggregate(
            today,
            start,
            rows.iter().map(|row| (row.date.as_deref(), row.spend)),
        )
    }

    /// Onglet « Semaine », appelé à la demande : coût par jour depuis le début de la période + projection.
    pub async fn fetch_week(
        &self,
        period_start: Option<&str>,
        budget_reset_at: Option<&str>,
        period_spend: f64,
    ) -> Result<WeekDetails, String> {
        let now = Utc::now();
        let today = now.date_naive();
        let start = period_start
            .and_then(parse_date)
            .unwrap_or(today - Duration::days(6));
        let summary = self
            .summary(format!(
                "/spend/logs?summarize=true&start_date={start}&end_date={}",
                today + Duration::days(1)
            ))
            .await?;
        Ok(WeekDetails {
            days: aggregate_days(&summary, start, today),
            projection: period_start
                .and_then(parse_instant)
                .zip(budget_reset_at.and_then(parse_instant))
                .and_then(|(start, reset)| project_instants(period_spend, start, reset, now)),
        })
    }
}

/// Enveloppe UTC couvrant la journée locale, même si sa durée n'est pas de 24 h.
fn day_range<T: chrono::TimeZone>(day: NaiveDate, zone: &T) -> Option<(NaiveDate, NaiveDate)> {
    let next = day.succ_opt()?;
    let start = zone
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .earliest();
    let end = zone
        .from_local_datetime(&next.and_hms_opt(0, 0, 0)?)
        .earliest();
    match start.zip(end) {
        Some((start, end)) => {
            let end = end.with_timezone(&Utc);
            let end_date = end.date_naive();
            let exclusive_end = if end.time() == chrono::NaiveTime::MIN {
                end_date
            } else {
                end_date.succ_opt()?
            };
            Some((start.with_timezone(&Utc).date_naive(), exclusive_end))
        }
        // Un saut à minuit peut rendre la frontière locale inexistante.
        None => Some((day.pred_opt()?, next.succ_opt()?)),
    }
}

/// Message d'erreur avec la chaîne de causes, sans l'URL.
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut msg = format!("requête échouée: {e}");
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(&format!(" ← {s}"));
        src = s.source();
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn client_rejects_blank_endpoints_before_any_request() {
        for url in ["", "   "] {
            assert!(
                matches!(Client::new(url, "test-key".into()), Err(e) if e.contains("Endpoint LiteLLM manquant"))
            );
        }
    }

    #[test]
    fn client_trims_a_configured_endpoint() {
        let client = Client::new("  https://example.com/llm/  ", "test-key".into()).unwrap();
        assert_eq!(client.base_url, "https://example.com/llm");
    }

    fn row(json: &str) -> LogRow {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn percentiles() {
        let v = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(percentile_sorted(&v, 50.0), Some(30.0));
        assert_eq!(percentile_sorted(&v, 95.0), Some(50.0));
        assert_eq!(percentile_sorted(&[], 50.0), None);
    }

    #[test]
    fn prompt_cache_metadata_is_optional_and_validated() {
        use serde_json::json;
        let valid = json!({"usage_object":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":80,"cache_write_tokens":20}}});
        for metadata in [valid.clone(), json!(valid.to_string())] {
            assert_eq!(
                row(&json!({"metadata":metadata}).to_string()).prompt_cache,
                Some((100, 80))
            );
        }
        for metadata in [
            json!(null),
            json!("invalid"),
            json!([]),
            json!({}),
            json!({"usage_object":{}}),
        ] {
            assert_eq!(
                row(&json!({"metadata":metadata}).to_string()).prompt_cache,
                None
            );
        }
        for cached in [
            json!(-1),
            json!(101),
            json!(1.5),
            json!("80"),
            json!(null),
            json!(true),
        ] {
            let mut metadata = valid.clone();
            metadata["usage_object"]["prompt_tokens_details"]["cached_tokens"] = cached;
            assert_eq!(
                row(&json!({"metadata":metadata}).to_string()).prompt_cache,
                None
            );
        }
        assert_eq!(row("{}").prompt_cache, None);
        let zero = json!({"metadata":{"usage_object":{"prompt_tokens":0,"prompt_tokens_details":{"cached_tokens":0}}}});
        assert_eq!(row(&zero.to_string()).prompt_cache, Some((0, 0)));
    }

    #[test]
    fn prompt_cache_weights_tokens_and_tracks_partial_coverage_on_local_day() {
        use serde_json::json;
        let make = |model: &str, input, cached, status, hit, time| {
            row(&json!({
            "model":model, "startTime":time, "status":status, "cache_hit":hit,
            "metadata":{"usage_object":{"prompt_tokens":input,"prompt_tokens_details":{"cached_tokens":cached}}}
        }).to_string())
        };
        let time = "2026-10-12T23:30:00Z";
        let rows = vec![
            make("openai/a", 100, Some(100), "success", false, time),
            make("a", 900, Some(0), "success", false, time),
            make("b", 200, None, "success", false, time),
            make("a", 500, Some(500), "failure", false, time),
            make("a", 500, Some(500), "success", true, time),
            make(
                "a",
                500,
                Some(500),
                "success",
                false,
                "2026-10-12T20:00:00Z",
            ),
        ];
        let timezone = chrono::FixedOffset::east_opt(7200).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        let activity = aggregate_activity(&rows, day, &timezone);
        assert_eq!(
            activity.prompt_cache,
            shared::PromptCacheUsage {
                input_tokens: 1000,
                cached_tokens: 100,
                reported_requests: 2,
                eligible_requests: 3,
            }
        );
        assert_eq!(activity.prompt_cache.rate(), Some(0.1));
        let a = activity.per_model.iter().find(|m| m.model == "a").unwrap();
        assert_eq!(a.prompt_cache.reported_requests, 2);
        assert_eq!(a.prompt_cache.eligible_requests, 2);
        let b = activity.per_model.iter().find(|m| m.model == "b").unwrap();
        assert_eq!(b.prompt_cache.reported_requests, 0);
        assert_eq!(b.prompt_cache.eligible_requests, 1);
    }

    #[test]
    fn activity_stats() {
        let now = Utc::now();
        let t = |ms: i64| (now + Duration::milliseconds(ms)).to_rfc3339();
        let rows = vec![
            row(&format!(
                r#"{{"model":"openai/m","spend":1.0,"total_tokens":100,"status":"success","cache_hit":"False",
                "startTime":"{}","endTime":"{}","completionStartTime":"{}"}}"#,
                t(0),
                t(2000),
                t(500)
            )),
            row(&format!(
                r#"{{"model":"m","spend":3.0,"total_tokens":300,"status":"success","cache_hit":true,
                "startTime":"{}","endTime":"{}"}}"#,
                t(0),
                t(4000)
            )),
            row(&format!(
                r#"{{"model":"","spend":0.0,"total_tokens":0,"status":"failure","cache_hit":"None","startTime":"{}"}}"#,
                t(0)
            )),
        ];
        let a = aggregate_activity(&rows, Local::now().date_naive(), &Local);
        assert_eq!((a.requests, a.errors), (3, 1));
        assert!((a.avg_cost - 4.0 / 3.0).abs() < 1e-9); // 4 $ sur 2 requêtes réussies
        assert_eq!(a.avg_tokens, 400.0 / 3.0);
        assert_eq!(a.cache_rate, Some(0.5)); // la valeur "None" n'est pas comptée
        assert_eq!(a.latency_p50_ms, Some(4000.0));
        assert_eq!(a.ttft_p50_ms, Some(500.0));
        assert_eq!(a.per_model.len(), 1);
        assert_eq!(a.per_model[0].requests, 2);
        assert_eq!(a.top_request.as_ref().unwrap().spend, 3.0);
        assert_eq!(a.hourly.iter().map(|h| h.requests).sum::<u64>(), 3);
    }

    #[test]
    fn week_days_are_filled_and_projection_extrapolates() {
        let start = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 10, 8).unwrap();
        let summary: Vec<DaySummary> = serde_json::from_str(
            r#"[{"startTime":"2026-10-05","spend":0.0,"models":{}},{"startTime":"2026-10-07","spend":7.5,"models":{}},
                {"startTime":"2026-10-08","spend":21.0,"models":{}}]"#,
        )
        .unwrap();
        let days = aggregate_days(&summary, start, today);
        assert_eq!(days.len(), 4);
        assert_eq!(days[1].spend, 0.0); // 2026-10-06 absent
        assert_eq!(days[3].spend, 21.0);

        let reset = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-08T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 3,5 jours écoulés sur 7 : 28 $ -> 56 $
        assert!((project(28.0, start, reset, now).unwrap() - 56.0).abs() < 1e-9);
        let early = DateTime::parse_from_rfc3339("2026-10-05T03:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(project(1.0, start, reset, early), None);
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_model("openai/gpt-6-sol"), Some("gpt-6-sol"));
        assert_eq!(
            normalize_model("bedrock/invoke/eu.anthropic.x"),
            Some("eu.anthropic.x")
        );
        assert_eq!(normalize_model("gpt-6-sol"), Some("gpt-6-sol"));
        assert_eq!(normalize_model(""), None);
    }

    #[test]
    fn sums_totals_across_days_and_aliases() {
        let days: Vec<DaySummary> = serde_json::from_str(
            r#"[{"startTime":"2026-10-06","spend":1.0,"models":{"openai/gpt-6-sol":1.0,"":0.0}},
                {"startTime":"2026-10-07","spend":2.5,"models":{"gpt-6-sol":2.5,"other":0.5}}]"#,
        )
        .unwrap();
        let totals = aggregate_totals(&days);
        assert_eq!(totals.len(), 2);
        assert_eq!(
            totals[0],
            ModelCost {
                model: "gpt-6-sol".into(),
                spend: 3.5
            }
        );
    }

    #[test]
    fn aggregates_today_only() {
        let now = Utc::now().to_rfc3339();
        let old = (Utc::now() - Duration::days(3)).to_rfc3339();
        let rows: Vec<LogRow> = serde_json::from_str(&format!(
            r#"[{{"model":"openai/m","spend":0.5,"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"startTime":"{now}"}},
                {{"model":"m","spend":0.25,"prompt_tokens":5,"completion_tokens":1,"total_tokens":6,"startTime":"{now}"}},
                {{"model":"m","spend":9.0,"prompt_tokens":1,"completion_tokens":1,"total_tokens":2,"startTime":"{old}"}}]"#
        ))
        .unwrap();
        let today = aggregate_today(&rows, Local::now().date_naive(), &Local);
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].requests, 2);
        assert_eq!(today[0].total_tokens, 18);
        assert!((today[0].spend - 0.75).abs() < 1e-9);
    }

    pub(super) fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn historical_range_covers_east_west_and_utc_days() {
        for (offset, start, end) in [
            (7200, "2024-02-28", "2024-03-01"),
            (-7200, "2024-02-29", "2024-03-02"),
            (0, "2024-02-29", "2024-03-01"),
        ] {
            assert_eq!(
                day_range(
                    d("2024-02-29"),
                    &chrono::FixedOffset::east_opt(offset).unwrap()
                )
                .unwrap(),
                (d(start), d(end))
            );
        }
    }

    #[derive(Clone)]
    pub(super) struct ShiftZone {
        pub(super) at: chrono::NaiveDateTime,
        pub(super) before: chrono::FixedOffset,
        pub(super) after: chrono::FixedOffset,
    }

    impl chrono::TimeZone for ShiftZone {
        type Offset = chrono::FixedOffset;
        fn from_offset(offset: &Self::Offset) -> Self {
            Self {
                at: chrono::NaiveDateTime::MAX,
                before: *offset,
                after: *offset,
            }
        }
        fn offset_from_local_date(&self, date: &NaiveDate) -> chrono::LocalResult<Self::Offset> {
            self.offset_from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
        }
        fn offset_from_utc_date(&self, date: &NaiveDate) -> Self::Offset {
            self.offset_from_utc_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
        }
        fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> Self::Offset {
            if *utc < self.at {
                self.before
            } else {
                self.after
            }
        }
        fn offset_from_local_datetime(
            &self,
            local: &chrono::NaiveDateTime,
        ) -> chrono::LocalResult<Self::Offset> {
            use chrono::LocalResult;
            let before_valid =
                *local - Duration::seconds(self.before.local_minus_utc().into()) < self.at;
            let after_valid =
                *local - Duration::seconds(self.after.local_minus_utc().into()) >= self.at;
            match (before_valid, after_valid) {
                (true, true) => LocalResult::Ambiguous(self.before, self.after),
                (true, false) => LocalResult::Single(self.before),
                (false, true) => LocalResult::Single(self.after),
                (false, false) => LocalResult::None,
            }
        }
    }

    #[test]
    fn historical_range_covers_dst_and_missing_or_repeated_midnight() {
        for (day, hour, before, after, start, end) in [
            ("2024-03-31", 1, 3600, 7200, "2024-03-30", "2024-04-01"),
            ("2024-10-27", 1, 7200, 3600, "2024-10-26", "2024-10-28"),
            ("2024-03-31", 0, 0, 3600, "2024-03-30", "2024-04-02"),
            ("2024-10-27", 0, 3600, 0, "2024-10-26", "2024-10-28"),
        ] {
            let zone = ShiftZone {
                at: d(day).and_hms_opt(hour, 0, 0).unwrap(),
                before: chrono::FixedOffset::east_opt(before).unwrap(),
                after: chrono::FixedOffset::east_opt(after).unwrap(),
            };
            assert_eq!(day_range(d(day), &zone).unwrap(), (d(start), d(end)));
            // Both repeated hours remain in the selected day; adjacent days do not.
            let rows: Vec<_> = [-1, 0, 1, 24, 48]
                .into_iter()
                .map(|hour| {
                    row(&serde_json::json!({"model":"m", "spend":1,
                        "startTime": (zone.at.and_utc() + Duration::hours(hour)).to_rfc3339()
                    })
                    .to_string())
                })
                .collect();
            let expected = if before == 0 { 2 } else { 3 };
            assert_eq!(aggregate_today(&rows, d(day), &zone)[0].requests, expected);
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn durations_are_exact_and_checked() {
        for (input, seconds) in [
            ("48h", 172800),
            ("90m", 5400),
            ("2w", 1209600),
            ("1mo", 2592000),
        ] {
            assert_eq!(parse_duration(input).unwrap().num_seconds(), seconds);
        }
        for input in ["", "é", "0d", "3D", "9223372036854775807w"] {
            assert!(parse_duration(input).is_none());
        }
    }

    #[test]
    fn period_start_subtracts_the_budget_duration() {
        let info = |reset: Option<&str>, dur: Option<&str>| KeyInfoInner {
            spend: 0.0,
            max_budget: None,
            budget_reset_at: reset.map(str::to_string),
            budget_duration: dur.map(str::to_string),
        };
        assert_eq!(
            period_start(&info(Some("2026-10-12T00:00:00Z"), Some("7d"))).as_deref(),
            Some("2026-10-05T00:00:00+00:00")
        );
        assert_eq!(
            period_start(&info(Some("2026-10-12T00:00:00Z"), None)).as_deref(),
            Some("2026-10-05T00:00:00+00:00")
        );
        assert_eq!(
            period_start(&info(Some("2026-10-31T00:00:00Z"), Some("1mo"))).as_deref(),
            Some("2026-10-01T00:00:00+00:00")
        );
        assert_eq!(period_start(&info(None, Some("7d"))), None);
        assert_eq!(period_start(&info(Some("pas une date"), Some("7d"))), None);
        assert_eq!(period_start(&info(Some("2026"), Some("7d"))), None);
    }

    #[test]
    fn project_edge_cases() {
        let start = d("2026-10-01");
        let reset = d("2026-10-11");
        // 5 jours écoulés sur 10 : on double.
        let p = project(50.0, start, reset, at("2026-10-06T00:00:00Z")).unwrap();
        assert!((p - 100.0).abs() < 1e-9);
        // Dépense nulle.
        assert_eq!(
            project(0.0, start, reset, at("2026-10-06T00:00:00Z")),
            Some(0.0)
        );
        // Moins de 6 h écoulées (frontière incluse à exactement 6 h).
        assert_eq!(project(1.0, start, reset, at("2026-10-01T05:59:59Z")), None);
        assert!(project(1.0, start, reset, at("2026-10-01T06:00:00Z")).is_some());
        // Maintenant avant le début, ou période vide / inversée.
        assert_eq!(project(1.0, start, reset, at("2026-09-30T00:00:00Z")), None);
        assert_eq!(project(1.0, start, start, at("2026-10-06T00:00:00Z")), None);
        assert_eq!(project(1.0, reset, start, at("2026-10-12T00:00:00Z")), None);
    }

    #[test]
    fn parse_date_needs_a_full_iso_prefix() {
        assert_eq!(parse_date("2026-10-12T00:00:00Z"), Some(d("2026-10-12")));
        assert_eq!(parse_date("2026-10-12"), Some(d("2026-10-12")));
        assert_eq!(parse_date("2026-10"), None);
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("éééééééééééé"), None); // pas de panique sur un découpage hors frontière
    }

    #[test]
    fn aggregate_days_caps_at_14_days_and_sums_duplicates() {
        let day = |date: &str, spend: f64| DaySummary {
            models: HashMap::new(),
            spend,
            start_time: Some(format!("{date}T00:00:00Z")),
        };
        let summary = [
            day("2026-10-20", 1.0),
            day("2026-10-20", 2.0),
            day("2026-10-21", 0.5),
        ];
        let days = aggregate_days(&summary, d("2026-09-01"), d("2026-10-21"));
        assert_eq!(days.len(), 14);
        assert_eq!(days[0].date, "2026-10-08");
        assert!((days[12].spend - 3.0).abs() < 1e-9);
        assert!((days[13].spend - 0.5).abs() < 1e-9);
        assert_eq!(days[1].spend, 0.0);
        // `start` après `today` : liste vide, sans panique.
        assert!(aggregate_days(&summary, d("2026-10-22"), d("2026-10-21")).is_empty());
        // Entrée sans date ignorée.
        let nodate = [DaySummary {
            models: HashMap::new(),
            spend: 9.0,
            start_time: None,
        }];
        assert!(aggregate_days(&nodate, d("2026-10-21"), d("2026-10-21"))
            .iter()
            .all(|x| x.spend == 0.0));
    }

    #[test]
    fn normalize_model_edge_cases() {
        assert_eq!(normalize_model("a/"), None);
        assert_eq!(normalize_model("   "), None);
        assert_eq!(normalize_model(""), None);
        assert_eq!(normalize_model(" bedrock/invoke/x "), Some("x"));
    }

    #[test]
    fn log_row_cache_hit_variants() {
        let cache = |v: &str| row(&format!(r#"{{"cache_hit":{v}}}"#)).cache();
        assert_eq!(cache("true"), Some(true));
        assert_eq!(cache("false"), Some(false));
        assert_eq!(cache(r#""True""#), Some(true));
        assert_eq!(cache(r#""False""#), Some(false));
        assert_eq!(cache(r#""None""#), None);
        assert_eq!(cache("null"), None);
        assert_eq!(row("{}").cache(), None);
    }

    #[test]
    fn log_row_tolerates_missing_fields_and_computes_latencies() {
        let r = row("{}");
        assert!(
            !r.failed()
                && r.start_time.is_none()
                && r.latency_ms().is_none()
                && r.ttft_ms().is_none()
        );
        let r = row(
            r#"{"status":"failure","startTime":"2026-10-12T10:00:00Z","completionStartTime":"2026-10-12T10:00:01Z","endTime":"2026-10-12T10:00:03Z"}"#,
        );
        assert!(r.failed());
        assert_eq!(r.latency_ms(), Some(3000.0));
        assert_eq!(r.ttft_ms(), Some(1000.0));
        // Durées négatives ou nulles ignorées.
        let r = row(r#"{"startTime":"2026-10-12T10:00:00Z","endTime":"2026-10-12T10:00:00Z"}"#);
        assert_eq!(r.latency_ms(), None);
    }

    #[test]
    fn totals_ignore_empty_model_names() {
        let days: Vec<DaySummary> = serde_json::from_str(
            r#"[{"models":{"":1.0,"a/m":2.0},"spend":3.0,"startTime":"2026-10-12"}]"#,
        )
        .unwrap();
        let totals = aggregate_totals(&days);
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].model, "m");
    }

    #[test]
    fn paid_failures_count_in_cost_and_token_averages() {
        let rows = vec![
            row(
                r#"{"model":"m","spend":2,"total_tokens":20,"status":"success","startTime":"2026-10-08T12:00:00Z","endTime":"2026-10-08T12:00:01Z"}"#,
            ),
            row(
                r#"{"model":"m","spend":4,"total_tokens":40,"status":"failure","startTime":"2026-10-08T12:00:00Z","endTime":"2026-10-08T12:00:10Z"}"#,
            ),
        ];
        let data = aggregate_activity(&rows, d("2026-10-08"), &Utc);
        assert_eq!((data.avg_cost, data.avg_tokens), (3.0, 30.0));
        assert_eq!(data.per_model[0].avg_cost, 3.0);
        assert_eq!(data.latency_p95_ms, Some(1000.0));
        let failed = aggregate_activity(&rows[1..], d("2026-10-08"), &Utc);
        assert_eq!(failed.avg_cost, 4.0);
        assert_eq!(failed.latency_p50_ms, None);
        assert_eq!(aggregate_activity(&[], d("2026-10-08"), &Utc).avg_cost, 0.0);
    }
    #[test]
    fn local_day_and_hour_are_explicit_at_midnight_and_dst_offsets() {
        let rows = vec![row(
            r#"{"model":"m","spend":1,"startTime":"2026-10-24T22:30:00Z"}"#,
        )];
        let east = chrono::FixedOffset::east_opt(7200).unwrap();
        let west = chrono::FixedOffset::west_opt(7200).unwrap();
        assert_eq!(
            aggregate_activity(&rows, d("2026-10-25"), &east).hourly[0].requests,
            1
        );
        assert_eq!(
            aggregate_activity(&rows, d("2026-10-25"), &west).requests,
            0
        );
        assert_eq!(aggregate_today(&rows, d("2026-10-25"), &east).len(), 1);
        let reset = at("2026-10-25T04:00:00Z");
        assert_eq!(
            reset - parse_duration("48h").unwrap(),
            at("2026-10-23T04:00:00Z")
        );
    }

    mod http {
        use super::super::*;
        use httpmock::prelude::*;

        #[tokio::test]
        async fn historical_day_filters_local_boundaries_and_aggregates_models() {
            let server = MockServer::start();
            let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
            let midnight = day
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_local_timezone(Local)
                .earliest()
                .unwrap();
            let next = day
                .succ_opt()
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_local_timezone(Local)
                .earliest()
                .unwrap();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/llm/spend/logs")
                    .header("authorization", "Bearer sk-test")
                    .query_param("summarize", "false");
                then.status(200).json_body(serde_json::json!([
                    {"model":"m", "spend":99, "total_tokens":999, "startTime":(midnight - Duration::seconds(1)).to_rfc3339()},
                    {"model":"openai/m", "spend":1.25, "prompt_tokens":10, "completion_tokens":2, "total_tokens":12, "startTime":midnight.to_rfc3339()},
                    {"model":"m", "spend":0.75, "prompt_tokens":20, "completion_tokens":3, "total_tokens":23, "startTime":(next - Duration::seconds(1)).to_rfc3339()},
                    {"model":"m", "spend":99, "total_tokens":999, "startTime":next.to_rfc3339()}
                ]));
            });
            let usage = client(&server)
                .fetch_day("2024-02-29", false)
                .await
                .unwrap();
            assert_eq!(usage.date, "2024-02-29");
            assert_eq!(
                usage.models,
                vec![ModelUsage {
                    model: "m".into(),
                    spend: 2.0,
                    requests: 2,
                    prompt_tokens: 30,
                    completion_tokens: 5,
                    total_tokens: 35,
                }]
            );
            mock.assert_hits(1);
        }

        #[tokio::test]
        async fn historical_day_rejects_invalid_and_future_dates_before_http() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.path("/llm/spend/logs");
                then.status(200).json_body(serde_json::json!([]));
            });
            let client = client(&server);
            let tomorrow = Local::now().date_naive().succ_opt().unwrap().to_string();
            for date in [
                "",
                "2025-02-29",
                "2024-2-29",
                "2024-02-29T00:00:00Z",
                " 2024-02-29",
                &tomorrow,
            ] {
                assert!(client.fetch_day(date, false).await.is_err(), "{date}");
            }
            mock.assert_hits(0);
            let result = client.fetch_day("2024-02-29", false).await.unwrap();
            assert_eq!(result.date, "2024-02-29");
            assert!(result.models.is_empty());
            mock.assert_hits(1);
        }

        #[tokio::test]
        async fn historical_navigation_does_not_wait_for_obsolete_requests() {
            let server = MockServer::start();
            let slow = server.mock(|when, then| {
                when.path("/llm/spend/logs").matches(|request| {
                    request.query_params.as_ref().is_some_and(|params| {
                        params
                            .iter()
                            .any(|(k, v)| k == "start_date" && v.starts_with("2023-"))
                    })
                });
                then.status(200)
                    .delay(std::time::Duration::from_millis(500))
                    .json_body(serde_json::json!([]));
            });
            let fast = server.mock(|when, then| {
                when.path("/llm/spend/logs").matches(|request| {
                    request.query_params.as_ref().is_some_and(|params| {
                        params
                            .iter()
                            .any(|(k, v)| k == "start_date" && v.starts_with("2024-"))
                    })
                });
                then.status(200).json_body(serde_json::json!([]));
            });
            let client = client(&server);
            let old_client = client.clone();
            let old = tokio::spawn(async move { old_client.fetch_day("2023-02-28", false).await });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while slow.hits_async().await == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let latest = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                client.fetch_day("2024-02-29", false),
            )
            .await
            .expect("the latest date must not wait for the obsolete HTTP response")
            .unwrap();
            assert_eq!(latest.date, "2024-02-29");
            old.await.unwrap().unwrap();
            assert_eq!(client.fetch_day("2024-02-29", false).await.unwrap(), latest);
            fast.assert_hits(1); // L'ancienne réponse ne remplace pas le cache de la dernière sélection.

            let old_client = client.clone();
            let old = tokio::spawn(async move { old_client.fetch_day("2023-02-28", true).await });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while slow.hits_async().await < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            client.fetch_day("2024-02-29", false).await.unwrap();
            old.await.unwrap().unwrap();
            client.fetch_day("2024-02-29", false).await.unwrap();
            fast.assert_hits(1); // Revenir au cache invalide aussi une réponse ancienne en transit.
        }

        #[tokio::test]
        async fn historical_cache_is_date_specific_and_force_bypasses_it() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.path("/llm/spend/logs")
                    .query_param("summarize", "false");
                then.status(200).json_body(serde_json::json!([]));
            });
            let client = client(&server);
            client.fetch_today().await.unwrap();
            client.fetch_day("2024-02-28", false).await.unwrap();
            client.fetch_day("2024-02-28", false).await.unwrap();
            mock.assert_hits(2);
            assert_eq!(
                client.fetch_day("2024-02-29", false).await.unwrap().date,
                "2024-02-29"
            );
            mock.assert_hits(3);
            client.fetch_day("2024-02-29", true).await.unwrap();
            mock.assert_hits(4);
            client.fetch_activity().await.unwrap();
            mock.assert_hits(4);
        }

        fn client(server: &MockServer) -> Client {
            Client::new(&format!("{}/llm/", server.base_url()), "sk-test".into()).unwrap()
        }

        #[tokio::test]
        async fn activity_reads_prompt_cache_without_extra_requests() {
            let server = MockServer::start();
            let request = server.mock(|when, then| {
                when.method(GET).path("/llm/spend/logs")
                    .header("authorization", "Bearer sk-test")
                    .query_param("summarize", "false");
                then.status(200).json_body(serde_json::json!([{
                    "model":"test-model", "startTime":Utc::now().to_rfc3339(),
                    "metadata":{"usage_object":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":75}}}
                }]));
            });
            let client = client(&server);
            let activity = client.fetch_activity().await.unwrap();
            assert_eq!(activity.prompt_cache.rate(), Some(0.75));
            assert_eq!(activity.per_model[0].prompt_cache.cached_tokens, 75);
            client.fetch_activity().await.unwrap();
            request.assert_hits(1);
        }

        #[tokio::test]
        async fn history_requests_30_days_and_rejects_invalid_history() {
            let server = MockServer::start();
            let day = NaiveDate::from_ymd_opt(2026, 1, 8).unwrap();
            let mut history = server.mock(|when, then| {
                when.path("/llm/spend/logs")
                    .header("authorization", "Bearer sk-test")
                    .query_param("summarize", "true")
                    .query_param("start_date", "2025-12-10")
                    .query_param("end_date", "2026-01-09");
                then.status(200).json_body(serde_json::json!([
                    {"startTime":"2025-12-18", "spend":20.0},
                    {"startTime":"2026-01-01", "spend":16.0},
                    {"startTime":"2026-01-08", "spend":500.0}
                ]));
            });
            let amounts = client(&server)
                .fetch_history(day, day - Duration::days(29))
                .await
                .unwrap();
            assert_eq!(amounts[8], Some(20.0));
            assert_eq!(amounts[22], Some(16.0));
            assert_eq!(amounts[29], Some(500.0));
            history.assert_hits(1);
            history.delete();
            server.mock(|when, then| {
                when.path("/llm/spend/logs");
                then.status(200)
                    .json_body(serde_json::json!([{ "spend": 3.0 }]));
            });
            assert!(client(&server)
                .fetch_history(day, day - Duration::days(29))
                .await
                .unwrap_err()
                .contains("date invalide"));
        }

        #[tokio::test]
        async fn concurrent_tabs_share_rows_and_expired_cache_is_reloaded() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.path("/llm/spend/logs")
                    .query_param("summarize", "false");
                then.status(200)
                    .delay(std::time::Duration::from_millis(20))
                    .json_body(serde_json::json!([]));
            });
            let client = client(&server);
            let (today, activity) = tokio::join!(client.fetch_today(), client.fetch_activity());
            assert!(today.is_ok() && activity.is_ok());
            mock.assert_hits(1);
            client.rows.lock().await.as_mut().unwrap().fetched -=
                std::time::Duration::from_secs(61);
            client.fetch_activity().await.unwrap();
            mock.assert_hits(2);
            client.rows.lock().await.as_mut().unwrap().day -= Duration::days(1);
            client.fetch_today().await.unwrap();
            mock.assert_hits(3);
        }
        #[tokio::test]
        async fn week_and_models_share_identical_summaries() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.path("/llm/spend/logs")
                    .query_param("summarize", "true");
                then.status(200).json_body(serde_json::json!([]));
            });
            let client = client(&server);
            let (models, week) = tokio::join!(
                client.fetch_models(Some("2026-10-01")),
                client.fetch_week(Some("2026-10-01"), None, 0.0)
            );
            assert!(models.is_ok() && week.is_ok());
            mock.assert_hits(1);
        }
        #[tokio::test]
        async fn fetch_key_parses_the_response_and_sends_the_bearer_token() {
            let server = MockServer::start();
            let m = server.mock(|when, then| {
                when.method(GET).path("/llm/key/info").header("authorization", "Bearer sk-test");
                then.status(200).json_body(serde_json::json!({
                    "info": {"spend": 12.5, "max_budget": 125.0,
                             "budget_reset_at": "2026-10-12T00:00:00.000000Z", "budget_duration": "7d"}
                }));
            });
            let k = client(&server).fetch_key().await.unwrap();
            m.assert();
            assert_eq!(k.period_spend, 12.5);
            assert_eq!(k.max_budget, Some(125.0));
            assert_eq!(
                k.budget_reset_at.as_deref(),
                Some("2026-10-12T00:00:00.000000Z")
            );
            assert_eq!(k.period_start.as_deref(), Some("2026-10-05T00:00:00+00:00"));
        }

        #[tokio::test]
        async fn fetch_key_without_budget_has_no_dates() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/llm/key/info");
                then.status(200)
                    .json_body(serde_json::json!({"info": {"spend": 1.0, "max_budget": null}}));
            });
            let k = client(&server).fetch_key().await.unwrap();
            assert_eq!(
                (k.max_budget, k.budget_reset_at, k.period_start),
                (None, None, None)
            );
        }

        #[tokio::test]
        async fn non_2xx_is_reported_with_the_status_and_path_only() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/llm/key/info");
                then.status(401);
            });
            let err = client(&server).fetch_key().await.err().unwrap();
            assert!(err.contains("401") && err.contains("/key/info"), "{err}");
            assert!(!err.contains("sk-test"));
        }

        #[tokio::test]
        async fn invalid_json_is_reported() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/llm/key/info");
                then.status(200).body("<html>oops</html>");
            });
            let err = client(&server).fetch_key().await.err().unwrap();
            assert!(err.starts_with("réponse invalide"), "{err}");
        }

        #[tokio::test]
        async fn unreachable_server_gives_a_message_without_the_url() {
            let c = Client::new("http://127.0.0.1:1", "k".into()).unwrap();
            let err = c.fetch_key().await.err().unwrap();
            assert!(err.starts_with("requête échouée"), "{err}");
            assert!(!err.contains("127.0.0.1:1/"), "{err}");
        }

        #[tokio::test]
        async fn fetch_models_uses_the_period_start_and_aggregates() {
            let server = MockServer::start();
            let m = server.mock(|when, then| {
                when.path("/llm/spend/logs").query_param("summarize", "true").query_param("start_date", "2026-10-05");
                then.status(200).json_body(serde_json::json!([
                    {"startTime": "2026-10-05", "spend": 3.0, "models": {"openai/a": 1.0, "b": 2.0}},
                    {"startTime": "2026-10-06", "spend": 1.0, "models": {"a": 1.0}}
                ]));
            });
            let models = client(&server)
                .fetch_models(Some("2026-10-05T00:00:00+00:00"))
                .await
                .unwrap();
            m.assert();
            assert_eq!(models.len(), 2);
            assert_eq!((models[0].model.as_str(), models[0].spend), ("a", 2.0));
            assert_eq!((models[1].model.as_str(), models[1].spend), ("b", 2.0));
        }

        #[tokio::test]
        async fn fetch_week_fills_days_from_the_period_start() {
            let server = MockServer::start();
            let start = (Utc::now().date_naive() - Duration::days(2)).to_string();
            let m = server.mock(|when, then| {
                when.path("/llm/spend/logs")
                    .query_param("start_date", start.as_str());
                then.status(200).json_body(
                    serde_json::json!([{"startTime": start, "spend": 2.0, "models": {}}]),
                );
            });
            let week = client(&server)
                .fetch_week(Some(&start), None, 2.0)
                .await
                .unwrap();
            m.assert();
            assert_eq!(week.days.len(), 3);
            assert_eq!(week.days[0].spend, 2.0);
            assert_eq!(week.projection, None);
        }
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Appel réel : `cargo test -p ember live -- --ignored --nocapture` (nécessite OPENAI_API_KEY et LITELLM_BASE_URL).
    #[tokio::test]
    #[ignore]
    async fn fetch_stats_live() {
        let c = Client::new(
            &std::env::var("LITELLM_BASE_URL")
                .expect("LITELLM_BASE_URL doit être définie pour le test réseau"),
            std::env::var("OPENAI_API_KEY").unwrap(),
        )
        .unwrap();
        let t = std::time::Instant::now();
        let key = c.fetch_key().await.unwrap();
        println!(
            "période ${:.2} depuis {:?} ({:?})",
            key.period_spend,
            key.period_start,
            t.elapsed()
        );
        let models = c.fetch_models(key.period_start.as_deref()).await.unwrap();
        println!("modèles ({:?})", t.elapsed());
        for m in &models {
            println!("{:<55} ${:.2}", m.model, m.spend);
        }
        let week = c
            .fetch_week(
                key.period_start.as_deref(),
                key.budget_reset_at.as_deref(),
                key.period_spend,
            )
            .await
            .unwrap();
        println!("--- semaine: projection {:?}", week.projection);
        for d in &week.days {
            println!("{} ${:.2}", d.date, d.spend);
        }
        let act = c.fetch_activity().await.unwrap();
        println!("--- activité ({:?})", t.elapsed());
        println!(
            "req {} err {} avg ${:.4} avg tok {:.0} cache {:?} p50 {:?} p95 {:?} ttft {:?}",
            act.requests,
            act.errors,
            act.avg_cost,
            act.avg_tokens,
            act.cache_rate,
            act.latency_p50_ms,
            act.latency_p95_ms,
            act.ttft_p50_ms
        );
        for m in &act.per_model {
            println!(
                "{:<50} {:>4} req {:>3} err ${:.4}/req p50 {:?}",
                m.model, m.requests, m.errors, m.avg_cost, m.latency_p50_ms
            );
        }
        println!(
            "heures: {:?}",
            act.hourly.iter().map(|h| h.requests).collect::<Vec<_>>()
        );
        println!("top: {:?}", act.top_request);
        let today = c.fetch_today().await.unwrap();
        println!("--- aujourd'hui ({:?})", t.elapsed());
        for u in &today {
            println!(
                "{:<55} {:>10} tok ${:.2} ({} req)",
                u.model, u.total_tokens, u.spend, u.requests
            );
        }
    }
}

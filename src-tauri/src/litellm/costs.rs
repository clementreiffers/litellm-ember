//! Coûts rapides : résumé UTC corrigé aux frontières de la journée locale.
use super::{
    aggregate_totals,
    calendar::{day_bounds, DayBounds},
    domain::{normalize_model, DaySummary},
    Client, Validated,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use sha2::{Digest, Sha256};
use shared::{DailyCosts, ModelCost};
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

struct CachedCosts {
    date: String,
    bounds: DayBounds,
    request_id: u64,
    fetched: Instant,
    validated_at: DateTime<Utc>,
    value: Option<DailyCosts>,
}
#[derive(Default)]
pub(super) struct CostState {
    sequence: u64,
    entries: VecDeque<CachedCosts>,
}

impl CostState {
    pub(super) fn compatible_usage(
        &self,
        bounds: DayBounds,
        usage: &shared::DailyUsage,
        validated_at: DateTime<Utc>,
    ) -> bool {
        self.entries
            .iter()
            .find(|entry| entry.bounds == bounds && entry.validated_at > validated_at)
            .and_then(|entry| entry.value.as_ref())
            .is_none_or(|costs| {
                costs.models.len() == usage.models.len()
                    && usage.models.iter().all(|m| {
                        costs
                            .models
                            .iter()
                            .any(|c| c.model == m.model && (c.spend - m.spend).abs() <= 1e-8)
                    })
            })
    }
}

impl Client {
    #[cfg(test)]
    pub async fn fetch_day_costs(&self, date: &str, force: bool) -> Result<DailyCosts, String> {
        self.fetch_day_costs_cached_in_zone(date, force, &Local)
            .await
    }

    #[cfg(test)]
    pub(super) async fn fetch_day_costs_cached_in_zone(
        &self,
        date: &str,
        force: bool,
        zone: &impl TimeZone,
    ) -> Result<DailyCosts, String> {
        let fresh_since = force.then(Instant::now);
        self.fetch_day_costs_validated(date, force, zone, fresh_since)
            .await
            .map(|v| v.data)
    }

    pub(crate) async fn fetch_day_costs_validated(
        &self,
        date: &str,
        force: bool,
        zone: &impl TimeZone,
        fresh_since: Option<Instant>,
    ) -> Result<Validated<DailyCosts>, String> {
        let started = Instant::now();
        let validated_at = Utc::now();
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .filter(|day| {
                date.len() == 10 && day.to_string() == date && *day <= Local::now().date_naive()
            })
            .ok_or("Date invalide ou future")?;
        let bounds = day_bounds(day, zone).ok_or("Frontière locale indisponible")?;
        if !force {
            if let Some(usage) = self.cached_day_validated(date, zone).await {
                return Ok(Validated {
                    fetched_at: usage.fetched_at,
                    data: DailyCosts {
                        date: usage.data.date,
                        models: usage
                            .data
                            .models
                            .into_iter()
                            .map(|m| ModelCost {
                                model: m.model,
                                spend: m.spend,
                            })
                            .collect(),
                    },
                });
            }
        }
        let id = {
            let mut state = self.costs.lock().await;
            let ttl = Duration::from_secs(if day == Local::now().date_naive() {
                60
            } else {
                900
            });
            if !force {
                if let Some(index) = state.entries.iter().position(|entry| {
                    entry.bounds == bounds && entry.value.is_some() && entry.fetched.elapsed() < ttl
                }) {
                    let entry = state.entries.remove(index).unwrap();
                    let value = entry.value.clone().unwrap();
                    let fetched_at = entry.validated_at;
                    state.entries.push_back(entry);
                    return Ok(Validated {
                        data: value,
                        fetched_at,
                    });
                }
            }
            state.sequence += 1;
            let id = state.sequence;
            let previous = state
                .entries
                .iter()
                .find(|e| e.bounds == bounds)
                .map(|e| (e.fetched, e.value.clone(), e.validated_at));
            state.entries.retain(|entry| entry.date != date);
            state.entries.push_back(CachedCosts {
                date: date.into(),
                bounds,
                request_id: id,
                fetched: previous.as_ref().map(|p| p.0).unwrap_or_else(Instant::now),
                validated_at: previous.as_ref().map(|p| p.2).unwrap_or(validated_at),
                value: previous.and_then(|p| p.1),
            });
            while state.entries.len() > 30 {
                state.entries.pop_front();
            }
            id
        };
        let value = self.fetch_day_costs_since(day, zone, fresh_since).await?;
        let mut state = self.costs.lock().await;
        if let Some(entry) = state.entries.iter_mut().find(|entry| {
            entry.date == date && entry.request_id == id && Some(bounds) == day_bounds(day, zone)
        }) {
            entry.value = Some(value.clone());
            entry.fetched = started;
            entry.validated_at = validated_at;
        }
        Ok(Validated {
            data: value,
            fetched_at: validated_at,
        })
    }

    #[cfg(test)]
    async fn fetch_day_costs_in_zone(
        &self,
        day: NaiveDate,
        zone: &impl TimeZone,
    ) -> Result<DailyCosts, String> {
        self.fetch_day_costs_since(day, zone, None).await
    }

    async fn fetch_day_costs_since(
        &self,
        day: NaiveDate,
        zone: &impl TimeZone,
        fresh_since: Option<Instant>,
    ) -> Result<DailyCosts, String> {
        let next = day.succ_opt().ok_or("Date hors limites")?;
        let midnight = day.and_hms_opt(0, 0, 0).unwrap();
        let next_midnight = next.and_hms_opt(0, 0, 0).unwrap();
        let DayBounds { start, end, .. } =
            day_bounds(day, zone).ok_or("Frontière locale indisponible")?;
        let utc_start = midnight.and_utc();
        let utc_end = next_midnight.and_utc();
        let summary = async {
            #[derive(serde::Deserialize)]
            struct CompleteSummary {
                #[serde(rename = "startTime")]
                start_time: String,
                spend: f64,
                models: HashMap<String, f64>,
            }
            let hash = format!("{:x}", Sha256::digest(self.api_key.as_bytes()));
            let rows: Vec<CompleteSummary> = self
                .get(&format!(
                    "/spend/logs?summarize=true&start_date={day}&end_date={next}&api_key={hash}"
                ))
                .await
                .map_err(String::from)?;
            if rows
                .iter()
                .any(|row| super::parse_date(&row.start_time).is_none())
            {
                return Err("Date du résumé de consommation invalide".into());
            }
            let mut selected = Vec::new();
            for row in rows
                .into_iter()
                .filter(|r| super::parse_date(&r.start_time) == Some(day))
            {
                let total: f64 = row.models.values().sum();
                if !row.spend.is_finite()
                    || row.spend < 0.0
                    || !total.is_finite()
                    || (total - row.spend).abs() > 1e-8
                    || row.models.iter().any(|(name, spend)| {
                        (normalize_model(name).is_none() && *spend != 0.0)
                            || !spend.is_finite()
                            || *spend < 0.0
                    })
                {
                    return Err("Résumé de consommation incomplet ou incohérent".into());
                }
                selected.push(DaySummary {
                    start_time: Some(row.start_time),
                    spend: row.spend,
                    models: row.models,
                });
            }
            Ok::<_, String>(aggregate_totals(&selected))
        };
        let (summary, start_costs, end_costs) = tokio::try_join!(
            summary,
            self.boundary_costs(start.min(utc_start), start.max(utc_start), fresh_since),
            self.boundary_costs(end.min(utc_end), end.max(utc_end), fresh_since),
        )?;
        let mut costs: HashMap<String, f64> =
            summary.into_iter().map(|m| (m.model, m.spend)).collect();
        for (models, sign) in [
            (start_costs, if start < utc_start { 1.0 } else { -1.0 }),
            (end_costs, if end > utc_end { 1.0 } else { -1.0 }),
        ] {
            for model in models {
                *costs.entry(model.model).or_default() += sign * model.spend;
            }
        }
        if costs.values().any(|v| !v.is_finite() || *v < -1e-8) {
            return Err("Résumé de consommation incohérent ; attente des détails".into());
        }
        let mut models: Vec<_> = costs
            .into_iter()
            .map(|(model, spend)| ModelCost {
                model,
                spend: spend.max(0.0),
            })
            .collect();
        models.sort_by(|a, b| b.spend.total_cmp(&a.spend).then(a.model.cmp(&b.model)));
        Ok(DailyCosts {
            date: day.to_string(),
            models,
        })
    }

    pub(super) async fn boundary_costs(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fresh_since: Option<Instant>,
    ) -> Result<Vec<ModelCost>, String> {
        if start == end {
            return Ok(vec![]);
        }
        let rows = self
            .shared_boundary_rows(start, end, fresh_since)
            .await?
            .ok_or("Correction de la journée locale indisponible")?;
        let mut costs = HashMap::new();
        for row in rows.iter() {
            if row.start_time < start || row.start_time >= end {
                continue;
            }
            if row.empty_failure() {
                continue;
            }
            let spend = row
                .spend
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or("Coût de frontière absent ou invalide")?;
            let model =
                normalize_model(&row.model).ok_or("Modèle de frontière absent ou invalide")?;
            let total = costs.entry(model.to_owned()).or_insert(0.0);
            *total += spend;
            if !total.is_finite() {
                return Err("Coût de frontière hors limites".into());
            }
        }
        Ok(costs
            .into_iter()
            .map(|(model, spend)| ModelCost { model, spend })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn daily_rounding_only_preview_preserves_memory_usage_without_download() {
        let server = MockServer::start();
        let logs=server.mock(|when,then|{when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[{"request_id":"a","model":"m","spend":23.301333660000004,"prompt_tokens":80,"completion_tokens":20,"total_tokens":100,"startTime":"2026-10-20T12:00:00Z"}],"page":1,"page_size":1000,"total":1,"total_pages":1}));});
        server.mock(|when,then|{when.path("/spend/logs");then.status(200).json_body(json!([{"startTime":"2026-10-20","spend":23.30133365999999,"models":{"m":23.30133365999999}}]));});
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let original = client
            .fetch_day_in_zone("2026-10-20", false, &Utc)
            .await
            .unwrap();
        client
            .fetch_day_costs_cached_in_zone("2026-10-20", true, &Utc)
            .await
            .unwrap();
        assert!(client
            .cached_day_in_zone("2026-10-20", &Utc)
            .await
            .is_some());
        assert_eq!(
            client
                .fetch_day_in_zone("2026-10-20", false, &Utc)
                .await
                .unwrap(),
            original
        );
        logs.assert_hits(1);
    }

    #[tokio::test]
    async fn daily_costs_accept_zero_empty_model_bucket_and_zero_failure_boundaries() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{"startTime":"2024-02-29","spend":5,"models":{"m":5,"":0}}]));
        });
        server.mock(|when,then|{when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[{"request_id":"failed","model":"","status":"failure","spend":0,"prompt_tokens":0,"completion_tokens":0,"total_tokens":0,"startTime":"2024-02-28T23:00:00Z"}],"total":1,"page":1,"page_size":1000,"total_pages":1}));});
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let value = client
            .fetch_day_costs_cached_in_zone(
                "2024-02-29",
                false,
                &chrono::FixedOffset::east_opt(7200).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(value.models.len(), 1);
        assert_eq!(value.models[0].spend, 5.0);
    }

    #[tokio::test]
    async fn daily_costs_reject_model_less_boundary() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{"startTime":"2024-02-29","spend":5,"models":{"m":5}}]));
        });
        server.mock(|when,then|{when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[{"request_id":"missing","spend":1,"startTime":"2024-02-28T23:00:00Z"}],"total":1,"page":1,"page_size":1000,"total_pages":1}));});
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        assert!(client
            .fetch_day_costs_cached_in_zone(
                "2024-02-29",
                false,
                &chrono::FixedOffset::east_opt(7200).unwrap()
            )
            .await
            .is_err());
    }

    use super::super::Client;
    use chrono::{NaiveDate, Utc};
    use httpmock::prelude::*;
    use serde_json::json;

    #[tokio::test]
    async fn daily_cost_failed_force_keeps_healthy_memory_value() {
        let server = MockServer::start();
        let mut valid = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{"startTime":"2024-02-29","spend":2,"models":{"m":2}}]));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let first = client
            .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
            .await
            .unwrap();
        valid.delete();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(503);
        });
        assert!(client
            .fetch_day_costs_cached_in_zone("2024-02-29", true, &Utc)
            .await
            .is_err());
        assert_eq!(
            client
                .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
                .await
                .unwrap(),
            first
        );
    }

    #[tokio::test]
    async fn daily_costs_reject_unreconciled_or_absent_model_breakdowns() {
        for row in [
            json!({"startTime":"2024-02-29","spend":100,"models":{"m":2}}),
            json!({"startTime":"2024-02-29","spend":100}),
            json!({"startTime":"2024-02-29","models":{"m":2}}),
            json!({"startTime":"2024-02-29","spend":1,"models":{"m":2,"n":-1}}),
        ] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/spend/logs");
                then.status(200).json_body(json!([row]));
            });
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            assert!(client
                .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn daily_caches_do_not_cross_calendar_boundaries() {
        let server = MockServer::start();
        let summary = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{"startTime":"2024-02-29","spend":2,"models":{"m":2}}]));
        });
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200)
                .json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        client
            .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
            .await
            .unwrap();
        client
            .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
            .await
            .unwrap();
        summary.assert_hits(1);
        client
            .fetch_day_costs_cached_in_zone(
                "2024-02-29",
                false,
                &chrono::FixedOffset::east_opt(7200).unwrap(),
            )
            .await
            .unwrap();
        summary.assert_hits(2);
        client
            .fetch_day_in_zone("2024-02-29", false, &Utc)
            .await
            .unwrap();
        assert!(client
            .cached_day_in_zone("2024-02-29", &Utc)
            .await
            .is_some());
        assert!(client
            .cached_day_in_zone("2024-02-29", &chrono::FixedOffset::east_opt(7200).unwrap())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn daily_cache_changed_bounds_reject_old_pending_result() {
        let server = MockServer::start();
        let mut old = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .delay(std::time::Duration::from_millis(250))
                .json_body(json!([{"startTime":"2024-02-29","spend":1,"models":{"m":1}}]));
        });
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200)
                .json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let cloned = client.clone();
        let pending = tokio::spawn(async move {
            cloned
                .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while old.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        old.delete();
        let current = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{"startTime":"2024-02-29","spend":2,"models":{"m":2}}]));
        });
        let zone = chrono::FixedOffset::east_opt(7200).unwrap();
        client
            .fetch_day_costs_cached_in_zone("2024-02-29", false, &zone)
            .await
            .unwrap();
        pending.await.unwrap().unwrap();
        assert_eq!(
            client
                .fetch_day_costs_cached_in_zone("2024-02-29", false, &zone)
                .await
                .unwrap()
                .models[0]
                .spend,
            2.0
        );
        current.assert_hits(1);
    }

    #[tokio::test]
    async fn daily_cost_preview_cache_expires_and_preserves_latest_forced_result() {
        let server = MockServer::start();
        let mut summary = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(json!([]));
        });
        server.mock(|when,then| {
            when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        client.fetch_day_costs("2024-02-29", false).await.unwrap();
        client
            .costs
            .lock()
            .await
            .entries
            .front_mut()
            .unwrap()
            .fetched -= std::time::Duration::from_secs(901);
        client.fetch_day_costs("2024-02-29", false).await.unwrap();
        summary.assert_hits(2);
        summary.delete();
        let mut old = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .delay(std::time::Duration::from_millis(250))
                .json_body(json!([{ "startTime":"2024-02-29","spend":1,"models":{"m":1}}]));
        });
        let cloned = client.clone();
        let pending = tokio::spawn(async move { cloned.fetch_day_costs("2024-02-29", true).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while old.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        old.delete();
        let corrected = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{ "startTime":"2024-02-29","spend":2,"models":{"m":2}}]));
        });
        client.fetch_day_costs("2024-02-29", true).await.unwrap();
        pending.await.unwrap().unwrap();
        assert_eq!(
            client
                .fetch_day_costs("2024-02-29", false)
                .await
                .unwrap()
                .models[0]
                .spend,
            2.0
        );
        corrected.assert_hits(1);
    }

    #[tokio::test]
    async fn daily_costs_dst_uses_distinct_midnight_offsets_and_rejects_missing_midnight() {
        use super::super::tests::{d, ShiftZone};
        for (day, before, after, first_start, last_start, first_end, last_end) in [
            (
                "2024-03-31",
                3600,
                7200,
                "2024-03-30 23:00:00",
                "2024-03-31 22:00:00",
                "2024-03-31 00:00:00",
                "2024-04-01 00:00:00",
            ),
            (
                "2024-10-27",
                7200,
                3600,
                "2024-10-26 22:00:00",
                "2024-10-27 23:00:00",
                "2024-10-27 00:00:00",
                "2024-10-28 00:00:00",
            ),
        ] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/spend/logs");
                then.status(200)
                    .json_body(json!([{ "startTime":day,"spend":10,"models":{"m":10}}]));
            });
            for (start, end, spend) in [(first_start, first_end, 3.0), (last_start, last_end, 1.0)]
            {
                server.mock(|when,then| {
                    when.path("/spend/logs/v2").query_param("start_date",start).query_param("end_date",end);
                    then.status(200).json_body(json!({"data":[{"request_id":"one","model":"m","spend":spend,"startTime":format!("{}Z",start.replace(' ',"T"))}],"total":1,"page":1,"page_size":1000,"total_pages":1,"total_is_capped":false}));
                });
            }
            let zone = ShiftZone {
                at: d(day).and_hms_opt(1, 0, 0).unwrap(),
                before: chrono::FixedOffset::east_opt(before).unwrap(),
                after: chrono::FixedOffset::east_opt(after).unwrap(),
            };
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            assert_eq!(
                client
                    .fetch_day_costs_in_zone(d(day), &zone)
                    .await
                    .unwrap()
                    .models[0]
                    .spend,
                12.0
            );
        }
        let server = MockServer::start();
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(500);
        });
        let zone = ShiftZone {
            at: d("2024-03-31").and_hms_opt(0, 0, 0).unwrap(),
            before: chrono::FixedOffset::east_opt(0).unwrap(),
            after: chrono::FixedOffset::east_opt(3600).unwrap(),
        };
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        assert!(client
            .fetch_day_costs_in_zone(d("2024-03-31"), &zone)
            .await
            .is_err());
        any.assert_hits(0);
    }

    #[tokio::test]
    async fn daily_cost_preview_is_cached_before_details_finish() {
        let server = MockServer::start();
        let summary = server.mock(|when, then| {
            when.path("/spend/logs").query_param("summarize", "true");
            then.status(200).json_body(json!([]));
        });
        server.mock(|when,then| {
            when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        client.fetch_day_costs("2024-02-28", false).await.unwrap();
        client.fetch_day_costs("2024-02-29", false).await.unwrap();
        client.fetch_day_costs("2024-02-28", false).await.unwrap();
        summary.assert_hits(2);
        client.fetch_day_costs("2024-02-28", true).await.unwrap();
        summary.assert_hits(3);
    }

    #[tokio::test]
    async fn daily_costs_correct_local_boundaries_and_merge_aliases() {
        for offset in [7200, -7200, 0] {
            let server = MockServer::start();
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            let zone = chrono::FixedOffset::east_opt(offset).unwrap();
            let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
            let summary = server.mock(|when, then| {
                when.path("/spend/logs")
                    .query_param("summarize", "true")
                    .query_param("start_date", "2024-02-29")
                    .query_param("end_date", "2024-03-01")
                    .query_param(
                        "api_key",
                        "f3abf2a6cc4f00987743db5f544ba345b4899ae31f326d8ee9c4816de153c9e0",
                    );
                then.status(200).json_body(json!([
                    {"startTime":"2024-02-29","spend":12,"models":{"openai/m":8,"m":2,"n":2}},
                    {"startTime":"2024-03-01","spend":100,"models":{"m":100}}
                ]));
            });
            let (start_a, end_a, start_b, end_b) = if offset > 0 {
                (
                    "2024-02-28 22:00:00",
                    "2024-02-29 00:00:00",
                    "2024-02-29 22:00:00",
                    "2024-03-01 00:00:00",
                )
            } else {
                (
                    "2024-02-29 00:00:00",
                    "2024-02-29 02:00:00",
                    "2024-03-01 00:00:00",
                    "2024-03-01 02:00:00",
                )
            };
            let corrections: Vec<_>=[(start_a,end_a,3.0),(start_b,end_b,1.0)].into_iter().map(|(start,end,spend)| server.mock(|when,then| {
                when.path("/spend/logs/v2").query_param("start_date",start).query_param("end_date",end);
                then.status(200).json_body(json!({"data":[
                    {"request_id":"included","model":"openai/m","spend":spend,"startTime":format!("{}Z",start.replace(' ',"T"))},
                    {"request_id":"excluded","model":"m","spend":99,"startTime":format!("{}Z",end.replace(' ',"T"))}
                ],"total":2,"page":1,"page_size":1000,"total_pages":1,"total_is_capped":false}));
            })).collect();
            let costs = client.fetch_day_costs_in_zone(day, &zone).await.unwrap();
            assert_eq!(costs.date, "2024-02-29");
            let model = costs.models.iter().find(|m| m.model == "m").unwrap();
            assert_eq!(
                model.spend,
                if offset > 0 {
                    12.0
                } else if offset < 0 {
                    8.0
                } else {
                    10.0
                }
            );
            assert_eq!(
                costs.models.iter().find(|m| m.model == "n").unwrap().spend,
                2.0
            );
            summary.assert_hits(1);
            for correction in corrections {
                correction.assert_hits(if offset == 0 { 0 } else { 1 });
            }
        }
    }

    #[tokio::test]
    async fn daily_costs_do_not_guess_when_boundary_details_are_unavailable() {
        for status in [403, 200] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/spend/logs");
                then.status(200)
                    .json_body(json!([{"startTime":"2024-02-29","spend":10,"models":{"m":10}}]));
            });
            server.mock(|when,then| {
                when.path("/spend/logs/v2"); then.status(status).json_body(json!({"data":[],"total":10000,"page":1,"page_size":1000,"total_pages":10,"total_is_capped":true}));
            });
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            assert!(client
                .fetch_day_costs_in_zone(
                    NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
                    &chrono::FixedOffset::east_opt(7200).unwrap()
                )
                .await
                .is_err());
        }
    }

    #[tokio::test]
    #[ignore]
    async fn daily_costs_live_match_details() {
        let client = Client::new(
            &std::env::var("LITELLM_BASE_URL").unwrap(),
            std::env::var("OPENAI_API_KEY").unwrap(),
        )
        .unwrap();
        let date = std::env::var("LITELLM_TEST_DATE").unwrap_or_else(|_| {
            chrono::Local::now()
                .date_naive()
                .pred_opt()
                .unwrap()
                .to_string()
        });
        let start = std::time::Instant::now();
        let costs = client.fetch_day_costs(&date, true).await.unwrap();
        let cost_time = start.elapsed();
        let start = std::time::Instant::now();
        let details = client.fetch_day(&date, true).await.unwrap();
        for model in &costs.models {
            let detailed = details
                .models
                .iter()
                .find(|m| m.model == model.model)
                .map(|m| m.spend)
                .unwrap_or(0.0);
            assert!((model.spend - detailed).abs() < 1e-8);
        }
        for model in &details.models {
            let preview = costs
                .models
                .iter()
                .find(|m| m.model == model.model)
                .map(|m| m.spend)
                .unwrap_or(0.0);
            assert!((model.spend - preview).abs() < 1e-8);
        }
        println!(
            "day={date} cost_preview={cost_time:?} full_details={:?} models={} total={}",
            start.elapsed(),
            costs.models.len(),
            costs.models.iter().map(|m| m.spend).sum::<f64>()
        );
        let original = client
            .fetch_day_validated(&date, false, &chrono::Local, None)
            .await
            .unwrap();
        // Reproduit un aperçu plus récent dont l'ordre des additions diffère des logs détaillés.
        client.fetch_day_costs(&date, true).await.unwrap();
        let revisit_started = std::time::Instant::now();
        let revisit = client
            .fetch_day_validated(&date, false, &chrono::Local, None)
            .await
            .unwrap();
        assert_eq!(revisit.fetched_at, original.fetched_at);
        assert_eq!(revisit.data, original.data);
        println!(
            "cached_revisit={:?}; validation timestamp unchanged",
            revisit_started.elapsed()
        );
    }

    #[tokio::test]
    async fn daily_costs_validate_dates_before_requests() {
        let server = MockServer::start();
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        for date in [
            "2025-02-29".into(),
            "2024-2-29".into(),
            (chrono::Local::now().date_naive() + chrono::Duration::days(1)).to_string(),
        ] {
            assert!(client
                .fetch_day_costs(&date, false)
                .await
                .unwrap_err()
                .contains("Date"));
        }
        let empty = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(json!([]));
        });
        assert!(client
            .fetch_day_costs_in_zone(NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(), &Utc)
            .await
            .unwrap()
            .models
            .is_empty());
        empty.assert_hits(1);
    }
}

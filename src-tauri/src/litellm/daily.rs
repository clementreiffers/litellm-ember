//! Chargement paginé et cache des journées de consommation.
use super::{
    calendar::{day_bounds, DayBounds},
    day_range,
    domain::normalize_model,
    Client, LogRow, TransportError, Validated,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use futures_util::{
    future::{BoxFuture, Shared},
    stream, FutureExt, StreamExt, TryStreamExt,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shared::DailyUsage;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

const PAGE_SIZE: usize = 1000;
const CACHE_DAYS: usize = 30;
const PAST_TTL: Duration = Duration::from_secs(15 * 60);

struct DailyCache {
    date: String,
    bounds: Option<DayBounds>,
    request_id: u64,
    fetched: Instant,
    validated_at: DateTime<Utc>,
    usage: Option<DailyUsage>,
}

#[derive(Default)]
pub(super) struct DailyState {
    request_id: u64,
    cache: VecDeque<DailyCache>,
}

type Request = Shared<BoxFuture<'static, Result<Validated<DailyUsage>, String>>>;
struct InFlight {
    bounds: DayBounds,
    force: bool,
    since: Option<Instant>,
    created: Instant,
    request: Request,
}
#[derive(Default)]
pub(super) struct Requests {
    entries: VecDeque<InFlight>,
}

/// Ignore les métadonnées, messages et réponses volumineux : seuls ces compteurs sont utiles.
#[derive(Deserialize)]
pub(super) struct UsageRow {
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    pub(super) model: String,
    pub(super) spend: Option<f64>,
    pub(super) prompt_tokens: Option<u64>,
    pub(super) completion_tokens: Option<u64>,
    pub(super) total_tokens: Option<u64>,
    status: Option<String>,
    #[serde(rename = "startTime")]
    pub(super) start_time: DateTime<Utc>,
}

impl UsageRow {
    /// LiteLLM journalise aussi les échecs survenus avant l'identification du modèle.
    pub(super) fn empty_failure(&self) -> bool {
        normalize_model(&self.model).is_none()
            && self.status.as_deref() == Some("failure")
            && self.spend == Some(0.0)
            && self.prompt_tokens == Some(0)
            && self.completion_tokens == Some(0)
            && self.total_tokens == Some(0)
    }
}

impl TryFrom<UsageRow> for LogRow {
    type Error = String;
    fn try_from(row: UsageRow) -> Result<Self, String> {
        let missing = "Compteurs de consommation absents ou invalides";
        let model = normalize_model(&row.model)
            .ok_or("Modèle de consommation absent ou invalide")?
            .to_owned();
        Ok(Self {
            model,
            spend: row
                .spend
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or(missing)?,
            prompt: row.prompt_tokens.ok_or(missing)?,
            completion: row.completion_tokens.ok_or(missing)?,
            total: row.total_tokens.ok_or(missing)?,
            start_time: Some(row.start_time),
            end_time: None,
            completion_start: None,
            status: None,
            cache_hit: None,
            prompt_cache: None,
        })
    }
}

#[derive(Deserialize)]
struct Page {
    data: Vec<UsageRow>,
    total: usize,
    page: usize,
    page_size: usize,
    total_pages: usize,
    #[serde(default)]
    total_is_capped: bool,
}

impl Page {
    fn validate(&self, page: usize, total: usize) -> Result<(), String> {
        let pages = total.div_ceil(PAGE_SIZE);
        let expected = total.saturating_sub((page - 1) * PAGE_SIZE).min(PAGE_SIZE);
        if self.page != page
            || self.page_size != PAGE_SIZE
            || self.total != total
            || self.total_pages != pages
            || self.data.len() != expected
            || self.total_is_capped
        {
            return Err("Pagination de consommation incomplète ou modifiée ; réessayez".into());
        }
        Ok(())
    }
}

impl Client {
    /// Les clics et le préchargement partagent la même validation ; une nouvelle force la remplace.
    pub(crate) async fn fetch_day_shared(
        &self,
        date: &str,
        force: bool,
        since: Option<Instant>,
    ) -> Result<Validated<DailyUsage>, String> {
        let bounds = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .filter(|d| d.to_string() == date && *d <= Local::now().date_naive())
            .and_then(|d| day_bounds(d, &Local))
            .ok_or("Frontière locale indisponible")?;
        let request = {
            let mut requests = self.daily_requests.lock().await;
            requests.entries.retain(|e| {
                e.request.peek().is_none()
                    || e.created.elapsed() < Duration::from_secs(60)
                        && !matches!(e.request.peek(), Some(Err(_)))
            });
            if let Some(entry) = requests.entries.iter().find(|e| {
                e.bounds == bounds
                    && if force {
                        e.force
                            && since
                                .map_or(e.request.peek().is_none(), |since| e.since == Some(since))
                    } else {
                        e.request.peek().is_none()
                    }
            }) {
                entry.request.clone()
            } else {
                let mut transport = self.clone();
                // Le futur ne doit pas retenir le cache qui le possède.
                transport.daily_requests = Default::default();
                let date = date.to_owned();
                let request = async move {
                    transport
                        .fetch_day_validated(
                            &date,
                            force,
                            &Local,
                            since.or_else(|| force.then(Instant::now)),
                        )
                        .await
                }
                .boxed()
                .shared();
                requests.entries.retain(|e| e.bounds != bounds);
                requests.entries.push_back(InFlight {
                    bounds,
                    force,
                    since,
                    created: Instant::now(),
                    request: request.clone(),
                });
                while requests.entries.len() > CACHE_DAYS {
                    requests.entries.pop_front();
                }
                request
            }
        };
        let result = request.clone().await;
        let mut requests = self.daily_requests.lock().await;
        if !requests
            .entries
            .iter()
            .any(|e| e.bounds == bounds && e.request.ptr_eq(&request))
        {
            return Err("La journée a été actualisée ; rechargez les détails".into());
        }
        if result.is_err() {
            requests.entries.retain(|e| !e.request.ptr_eq(&request));
        }
        result
    }
    #[cfg(test)]
    pub(super) async fn cached_day_in_zone(
        &self,
        date: &str,
        zone: &impl TimeZone,
    ) -> Option<DailyUsage> {
        self.cached_day_validated(date, zone).await.map(|v| v.data)
    }

    pub(super) async fn cached_day_validated(
        &self,
        date: &str,
        zone: &impl TimeZone,
    ) -> Option<Validated<DailyUsage>> {
        let bounds = day_bounds(NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?, zone)?;
        let costs = self.costs.lock().await;
        let state = self.daily.lock().await;
        let ttl = if date == Local::now().date_naive().to_string() {
            Duration::from_secs(60)
        } else {
            PAST_TTL
        };
        state
            .cache
            .iter()
            .find(|entry| {
                entry.bounds == Some(bounds)
                    && entry.fetched.elapsed() < ttl
                    && entry
                        .usage
                        .as_ref()
                        .is_some_and(|u| costs.compatible_usage(bounds, u, entry.validated_at))
            })
            .and_then(|entry| {
                entry.usage.clone().map(|data| Validated {
                    data,
                    fetched_at: entry.validated_at,
                })
            })
    }
    /// Réutilise les agrégats seulement pour la même connexion, jamais les lignes du jour courant.
    pub fn reuse_daily_cache(&mut self, previous: &Self) {
        if self.base_url == previous.base_url && self.api_key == previous.api_key {
            self.daily = previous.daily.clone();
            self.daily_requests = previous.daily_requests.clone();
            self.analytics = previous.analytics.clone();
            self.costs = previous.costs.clone();
            self.ranges = previous.ranges.clone();
        }
    }

    #[cfg(test)]
    pub async fn fetch_day(&self, date: &str, force: bool) -> Result<DailyUsage, String> {
        self.fetch_day_in_zone(date, force, &Local).await
    }

    #[cfg(test)]
    pub(super) async fn fetch_day_in_zone(
        &self,
        date: &str,
        force: bool,
        zone: &impl TimeZone,
    ) -> Result<DailyUsage, String> {
        self.fetch_day_validated(date, force, zone, force.then(Instant::now))
            .await
            .map(|v| v.data)
    }

    pub(crate) async fn fetch_day_validated(
        &self,
        date: &str,
        force: bool,
        zone: &impl TimeZone,
        fresh_since: Option<Instant>,
    ) -> Result<Validated<DailyUsage>, String> {
        let started = Instant::now();
        let validated_at = Utc::now();
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .filter(|day| date.len() == 10 && day.to_string() == date)
            .ok_or("Date invalide : utilisez YYYY-MM-DD")?;
        let today = Local::now().date_naive();
        if day > today {
            return Err("La date ne peut pas être dans le futur".into());
        }
        let bounds = day_bounds(day, zone);
        let request_id = {
            let costs = self.costs.lock().await;
            let mut state = self.daily.lock().await;
            let ttl = if day == today {
                Duration::from_secs(60)
            } else {
                PAST_TTL
            };
            if !force {
                if let Some(index) = state.cache.iter().position(|c| {
                    bounds.is_some()
                        && c.bounds == bounds
                        && c.usage.is_some()
                        && c.fetched.elapsed() < ttl
                        && c.usage.as_ref().is_some_and(|u| {
                            costs.compatible_usage(
                                bounds.expect("frontières vérifiées"),
                                u,
                                c.validated_at,
                            )
                        })
                }) {
                    let cached = state.cache.remove(index).unwrap();
                    let usage = cached.usage.clone().unwrap();
                    state.cache.push_back(cached);
                    return Ok(Validated {
                        data: usage,
                        fetched_at: state.cache.back().expect("entrée réinsérée").validated_at,
                    });
                }
            }
            state.request_id += 1;
            let request_id = state.request_id;
            let previous = state
                .cache
                .iter()
                .find(|c| c.bounds == bounds && bounds.is_some())
                .map(|c| (c.fetched, c.usage.clone(), c.validated_at));
            state.cache.retain(|c| c.date != date);
            state.cache.push_back(DailyCache {
                date: date.into(),
                bounds,
                request_id,
                fetched: previous.as_ref().map(|p| p.0).unwrap_or_else(Instant::now),
                validated_at: previous.as_ref().map(|p| p.2).unwrap_or(validated_at),
                usage: previous.and_then(|p| p.1),
            });
            while state.cache.len() > CACHE_DAYS {
                state.cache.pop_front();
            }
            request_id
        };
        let models = if let Some(models) = self
            .fetch_analytics_since(day, zone, force, fresh_since)
            .await
        {
            models
        } else {
            let (start, end) = exact_range(day, zone).ok_or("Date hors limites")?;
            let rows: Vec<UsageRow> = match self.fetch_range_usage_pages(start, end).await? {
                Some(rows) => rows,
                None => {
                    let (start, end) = day_range(day, zone).ok_or("Date hors limites")?;
                    let hash = format!("{:x}", Sha256::digest(self.api_key.as_bytes()));
                    self.get(&format!(
                        "/spend/logs?summarize=false&start_date={start}&end_date={end}&api_key={hash}"
                    ))
                    .await?
                }
            };
            let rows = rows
                .into_iter()
                .filter(|r| r.start_time.with_timezone(zone).date_naive() == day)
                .filter(|r| !r.empty_failure())
                .map(LogRow::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            aggregate_daily(&rows)?
        };
        // Les totaux affichés additionnent aussi les modèles ; valider cette somme avant publication.
        let mut totals = [0u64; 4];
        let mut spend = 0.0;
        for model in &models {
            for (sum, value) in totals.iter_mut().zip([
                model.requests,
                model.prompt_tokens,
                model.completion_tokens,
                model.total_tokens,
            ]) {
                *sum = sum
                    .checked_add(value)
                    .ok_or("Compteurs de consommation hors limites")?;
            }
            spend += model.spend;
        }
        if !spend.is_finite() {
            return Err("Coût total de consommation hors limites".into());
        }
        let usage = DailyUsage {
            date: date.into(),
            models,
        };
        let mut state = self.daily.lock().await;
        if let Some(entry) = state.cache.iter_mut().find(|c| {
            c.date == date
                && c.request_id == request_id
                && bounds.is_some()
                && bounds == day_bounds(day, zone)
        }) {
            entry.fetched = started;
            entry.validated_at = validated_at;
            entry.usage = Some(usage.clone());
        }
        Ok(Validated {
            data: usage,
            fetched_at: validated_at,
        })
    }

    // Conserve les champs absents pour refuser les corrections analytiques incomplètes.
    pub(super) async fn fetch_range_usage_pages(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Option<Vec<UsageRow>>, String> {
        let mut url = reqwest::Url::parse(&format!("{}/spend/logs/v2", self.base_url))
            .map_err(|_| "Endpoint invalide")?;
        let hash = format!("{:x}", Sha256::digest(self.api_key.as_bytes()));
        url.query_pairs_mut().extend_pairs([
            ("start_date", start.format("%Y-%m-%d %H:%M:%S").to_string()),
            ("end_date", end.format("%Y-%m-%d %H:%M:%S").to_string()),
            ("api_key", hash),
            ("page_size", PAGE_SIZE.to_string()),
            ("sort_by", "startTime".into()),
            ("sort_order", "asc".into()),
        ]);
        let path = format!("/spend/logs/v2?{}", url.query().unwrap());
        let first: Page = match self.get(&format!("{path}&page=1")).await {
            Ok(page) => page,
            Err(TransportError::Status { status, .. }) if matches!(status.as_u16(), 403..=405) => {
                return Ok(None)
            }
            Err(error) => return Err(error.into()),
        };
        // Le serveur plafonne certains comptes à 10 000 lignes : ne jamais afficher un total tronqué.
        if first.total_is_capped {
            return Ok(None);
        }
        first.validate(1, first.total)?;
        let total = first.total;
        let pages = first.total_pages;
        let mut rows = first.data;
        let mut remaining = stream::iter(2..=pages)
            .map(|number| {
                let path = &path;
                async move {
                    let page: Page = self
                        .get(&format!("{path}&page={number}"))
                        .await
                        .map_err(String::from)?;
                    page.validate(number, total)?;
                    Ok::<_, String>(page.data)
                }
            })
            .buffer_unordered(3);
        while let Some(page) = remaining.try_next().await? {
            rows.extend(page);
        }
        let mut ids = HashSet::new();
        if rows
            .iter()
            .any(|row| row.request_id.is_empty() || !ids.insert(&row.request_id))
        {
            return Err("Requêtes dupliquées dans la pagination ; réessayez".into());
        }
        Ok(Some(rows))
    }
}

fn aggregate_daily(rows: &[LogRow]) -> Result<Vec<shared::ModelUsage>, String> {
    let mut models = HashMap::<&str, shared::ModelUsage>::new();
    for row in rows {
        let model = models
            .entry(&row.model)
            .or_insert_with(|| shared::ModelUsage {
                model: row.model.clone(),
                ..Default::default()
            });
        let overflow = "Compteurs de consommation hors limites";
        model.requests = model.requests.checked_add(1).ok_or(overflow)?;
        model.prompt_tokens = model
            .prompt_tokens
            .checked_add(row.prompt)
            .ok_or(overflow)?;
        model.completion_tokens = model
            .completion_tokens
            .checked_add(row.completion)
            .ok_or(overflow)?;
        model.total_tokens = model.total_tokens.checked_add(row.total).ok_or(overflow)?;
        model.spend += row.spend;
        if !model.spend.is_finite() {
            return Err("Coût de consommation hors limites".into());
        }
    }
    let mut models: Vec<_> = models.into_values().collect();
    models.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then(a.model.cmp(&b.model))
    });
    Ok(models)
}

fn exact_range(day: NaiveDate, zone: &impl TimeZone) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let next = day.succ_opt()?;
    let start = zone
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .earliest();
    let end = zone
        .from_local_datetime(&next.and_hms_opt(0, 0, 0)?)
        .earliest();
    match start.zip(end) {
        Some((start, end)) => Some((start.with_timezone(&Utc), end.with_timezone(&Utc))),
        None => Some((
            day.pred_opt()?.and_hms_opt(0, 0, 0)?.and_utc(),
            next.succ_opt()?.and_hms_opt(0, 0, 0)?.and_utc(),
        )),
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn daily_ignores_only_explicit_zero_usage_failures_without_a_model() {
        for legacy in [false, true] {
            for fault in [
                "zero_failure",
                "nonzero_cost",
                "nonzero_tokens",
                "missing_counter",
                "unknown_status",
                "success",
            ] {
                let server = MockServer::start();
                let mut data = rows(0, 1);
                let mut failed = json!({"request_id":"failed","model":"","spend":0,"prompt_tokens":0,"completion_tokens":0,"total_tokens":0,"status":"failure","startTime":"2024-02-29T12:00:00Z"});
                match fault {
                    "nonzero_cost" => failed["spend"] = json!(1),
                    "nonzero_tokens" => failed["prompt_tokens"] = json!(1),
                    "missing_counter" => {
                        failed.as_object_mut().unwrap().remove("total_tokens");
                    }
                    "unknown_status" => {
                        failed.as_object_mut().unwrap().remove("status");
                    }
                    "success" => failed["status"] = json!("success"),
                    _ => {}
                }
                data.push(failed);
                server.mock(|when, then| {
                    when.path(if legacy {
                        "/llm/spend/logs"
                    } else {
                        "/llm/spend/logs/v2"
                    });
                    then.status(200).json_body(if legacy {
                        json!(data)
                    } else {
                        page(1, 2, data)
                    });
                });
                let result = client(&server)
                    .fetch_day_in_zone("2024-02-29", false, &Utc)
                    .await;
                if fault == "zero_failure" {
                    let value = result.unwrap();
                    assert_eq!(value.models.len(), 1);
                    assert_eq!(value.models[0].spend, 0.5);
                    assert_eq!(value.models[0].total_tokens, 12);
                    assert_eq!(value.models[0].requests, 1);
                } else {
                    assert!(result.is_err(), "{fault} legacy={legacy}");
                }
            }
        }
    }

    #[tokio::test]
    async fn daily_rejects_model_less_rows_and_aggregate_overflow() {
        for legacy in [false, true] {
            for fault in [
                "missing_model",
                "empty_model",
                "invalid_model",
                "prompt_overflow",
                "completion_overflow",
                "total_overflow",
                "spend_overflow",
            ] {
                let server = MockServer::start();
                let mut data = rows(0, 2);
                match fault {
                    "missing_model" => {
                        data[0].as_object_mut().unwrap().remove("model");
                    }
                    "empty_model" => data[0]["model"] = json!(""),
                    "invalid_model" => data[0]["model"] = json!("openai/"),
                    "prompt_overflow" => data[0]["prompt_tokens"] = json!(u64::MAX),
                    "completion_overflow" => data[0]["completion_tokens"] = json!(u64::MAX),
                    "total_overflow" => data[0]["total_tokens"] = json!(u64::MAX),
                    _ => {
                        data[0]["spend"] = json!(f64::MAX);
                        data[1]["spend"] = json!(f64::MAX);
                    }
                }
                server.mock(|when, then| {
                    when.path(if legacy {
                        "/llm/spend/logs"
                    } else {
                        "/llm/spend/logs/v2"
                    });
                    then.status(200).json_body(if legacy {
                        json!(data)
                    } else {
                        page(1, 2, data)
                    });
                });
                assert!(
                    client(&server)
                        .fetch_day_in_zone("2024-02-29", false, &Utc)
                        .await
                        .is_err(),
                    "{fault}, legacy={legacy}"
                );
            }
        }
    }

    use super::super::*;
    use super::{exact_range, PAST_TTL};
    use httpmock::prelude::*;
    use serde_json::{json, Value};

    fn client(server: &MockServer) -> Client {
        Client::new(&format!("{}/llm", server.base_url()), "sk-test".into()).unwrap()
    }

    fn rows(start: usize, count: usize) -> Vec<Value> {
        (start..start + count)
            .map(|id| {
                json!({
                    "request_id":format!("request-{id}"), "model":"openai/m",
                    "spend":0.5,"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,
                    "startTime":"2024-02-29T12:00:00Z",
                    "metadata":{"unused":"not needed for daily usage"}
                })
            })
            .collect()
    }

    fn page(number: usize, total: usize, data: Vec<Value>) -> Value {
        json!({"data":data,"page":number,"page_size":1000,"total":total,
            "total_pages":total.div_ceil(1000),"total_is_capped":false})
    }

    #[tokio::test]
    async fn daily_changed_costs_invalidate_memory_usage_outside_persisted_window() {
        let server = MockServer::start();
        let mut valid = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(200).json_body(page(1, 1, rows(0, 1)));
        });
        server.mock(|when, then| {
            when.path("/llm/spend/logs")
                .query_param("summarize", "true");
            then.status(200)
                .json_body(json!([{"startTime":"2024-02-29","spend":9,"models":{"m":9}}]));
        });
        let client = client(&server);
        client
            .fetch_day_in_zone("2024-02-29", false, &Utc)
            .await
            .unwrap();
        valid.delete();
        let failed = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(503);
        });
        assert_eq!(
            client
                .fetch_day_costs_cached_in_zone("2024-02-29", true, &Utc)
                .await
                .unwrap()
                .models[0]
                .spend,
            9.0
        );
        assert!(client
            .fetch_day_in_zone("2024-02-29", true, &Utc)
            .await
            .is_err());
        assert!(client
            .fetch_day_in_zone("2024-02-29", false, &Utc)
            .await
            .is_err());
        failed.assert_hits(2);
        assert_eq!(
            client
                .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
                .await
                .unwrap()
                .models[0]
                .spend,
            9.0
        );
    }

    #[tokio::test]
    async fn daily_memory_hits_preserve_original_validation_time() {
        let server = MockServer::start();
        let logs = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(200)
                .delay(std::time::Duration::from_millis(20))
                .json_body(page(1, 1, rows(0, 1)));
        });
        let client = client(&server);
        let first = client
            .fetch_day_validated("2024-02-29", false, &Utc, None)
            .await
            .unwrap();
        let second = client
            .fetch_day_validated("2024-02-29", false, &Utc, None)
            .await
            .unwrap();
        let costs = client
            .fetch_day_costs_validated("2024-02-29", false, &Utc, None)
            .await
            .unwrap();
        assert_eq!(first.fetched_at, second.fetched_at);
        assert_eq!(first.fetched_at, costs.fetched_at);
        logs.assert_hits(1);
        client.daily.lock().await.cache.front_mut().unwrap().fetched -= PAST_TTL;
        let third = client
            .fetch_day_validated("2024-02-29", false, &Utc, None)
            .await
            .unwrap();
        assert!(third.fetched_at > first.fetched_at);
        logs.assert_hits(2);
    }

    #[tokio::test]
    async fn daily_failed_force_keeps_healthy_memory_value() {
        let server = MockServer::start();
        let mut valid = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(200).json_body(page(1, 1, rows(0, 1)));
        });
        let client = client(&server);
        let first = client.fetch_day("2024-02-29", false).await.unwrap();
        valid.delete();
        server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(503);
        });
        assert!(client.fetch_day("2024-02-29", true).await.is_err());
        assert_eq!(client.fetch_day("2024-02-29", false).await.unwrap(), first);
    }

    #[tokio::test]
    async fn daily_legacy_fallback_preserves_key_scope() {
        let server = MockServer::start();
        let scoped = server.mock(|when, then| {
            when.path("/llm/spend/logs").query_param(
                "api_key",
                "f3abf2a6cc4f00987743db5f544ba345b4899ae31f326d8ee9c4816de153c9e0",
            );
            then.status(200).json_body(rows(0, 1));
        });
        let value = client(&server)
            .fetch_day_in_zone("2024-02-29", false, &Utc)
            .await
            .unwrap();
        assert_eq!(value.models[0].spend, 0.5);
        scoped.assert_hits(1);
    }

    #[tokio::test]
    async fn daily_rejects_missing_counters_and_invalid_spend_on_both_sources() {
        for legacy in [false, true] {
            for field in [
                "spend",
                "prompt_tokens",
                "completion_tokens",
                "total_tokens",
            ] {
                for null in [false, true] {
                    let server = MockServer::start();
                    let mut data = rows(0, 1);
                    if null {
                        data[0][field] = Value::Null;
                    } else {
                        data[0].as_object_mut().unwrap().remove(field);
                    }
                    if legacy {
                        server.mock(|when, then| {
                            when.path("/llm/spend/logs");
                            then.status(200).json_body(json!(data));
                        });
                    } else {
                        server.mock(|when, then| {
                            when.path("/llm/spend/logs/v2");
                            then.status(200).json_body(page(1, 1, data));
                        });
                    }
                    assert!(
                        client(&server)
                            .fetch_day_in_zone("2024-02-29", false, &Utc)
                            .await
                            .is_err(),
                        "legacy={legacy} field={field} null={null}"
                    );
                }
            }
            let server = MockServer::start();
            let mut data = rows(0, 1);
            data[0]["spend"] = json!(-1);
            server.mock(|when, then| {
                when.path(if legacy {
                    "/llm/spend/logs"
                } else {
                    "/llm/spend/logs/v2"
                });
                then.status(200).json_body(if legacy {
                    json!(data)
                } else {
                    page(1, 1, data)
                });
            });
            assert!(client(&server)
                .fetch_day_in_zone("2024-02-29", false, &Utc)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn historical_v2_uses_exact_local_bounds_and_aggregates_every_page() {
        let server = MockServer::start();
        let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let start = day
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        let end = day
            .succ_opt()
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        let first = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2")
                .header("authorization", "Bearer sk-test")
                .query_param(
                    "api_key",
                    "f3abf2a6cc4f00987743db5f544ba345b4899ae31f326d8ee9c4816de153c9e0",
                )
                .query_param("start_date", start.format("%Y-%m-%d %H:%M:%S").to_string())
                .query_param("end_date", end.format("%Y-%m-%d %H:%M:%S").to_string())
                .query_param("page_size", "1000")
                .query_param("page", "1")
                .query_param("sort_by", "startTime")
                .query_param("sort_order", "asc");
            then.status(200).json_body(page(1, 2001, rows(0, 1000)));
        });
        let second = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2").query_param("page", "2");
            then.status(200).json_body(page(2, 2001, rows(1000, 1000)));
        });
        let mut boundary = rows(2000, 1);
        boundary[0]["startTime"] = json!(end.to_rfc3339());
        let last = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2").query_param("page", "3");
            then.status(200).json_body(page(3, 2001, boundary));
        });
        let data = client(&server)
            .fetch_day("2024-02-29", false)
            .await
            .unwrap();
        assert_eq!(
            data.models,
            vec![ModelUsage {
                model: "m".into(),
                spend: 1000.0,
                requests: 2000,
                prompt_tokens: 20000,
                completion_tokens: 4000,
                total_tokens: 24000
            }]
        );
        first.assert_hits(1);
        second.assert_hits(1);
        last.assert_hits(1);
    }

    #[tokio::test]
    async fn historical_v2_loads_at_most_three_remaining_pages_concurrently() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/llm/spend/logs/v2").query_param("page", "1");
            then.status(200).json_body(page(1, 5000, rows(0, 1000)));
        });
        let pages: Vec<_> = (2..=5)
            .map(|number| {
                server.mock(|when, then| {
                    when.path("/llm/spend/logs/v2")
                        .query_param("page", number.to_string());
                    then.status(200)
                        .delay(std::time::Duration::from_millis(500))
                        .json_body(page(number, 5000, rows((number - 1) * 1000, 1000)));
                })
            })
            .collect();
        let client = client(&server);
        let fetch = tokio::spawn(async move { client.fetch_day("2024-02-29", false).await });
        tokio::time::timeout(std::time::Duration::from_millis(400), async {
            while pages[..3].iter().any(|p| p.hits() == 0) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("three pages must start before any delayed page completes");
        pages[3].assert_hits(0);
        assert_eq!(fetch.await.unwrap().unwrap().models[0].requests, 5000);
        for page in pages {
            page.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn historical_v2_rejects_partial_duplicate_or_inconsistent_pages_without_caching() {
        for fault in ["failed", "missing", "duplicate", "changed_total"] {
            let server = MockServer::start();
            let first = server.mock(|when, then| {
                when.path("/llm/spend/logs/v2").query_param("page", "1");
                then.status(200).json_body(page(1, 1001, rows(0, 1000)));
            });
            server.mock(|when, then| {
                when.path("/llm/spend/logs/v2").query_param("page", "2");
                if fault == "failed" {
                    then.status(503);
                } else {
                    let data = match fault {
                        "missing" => vec![],
                        "duplicate" => rows(0, 1),
                        _ => rows(1000, 1),
                    };
                    then.status(200).json_body(page(
                        2,
                        if fault == "changed_total" { 1002 } else { 1001 },
                        data,
                    ));
                }
            });
            let client = client(&server);
            assert!(
                client.fetch_day("2024-02-29", false).await.is_err(),
                "{fault}"
            );
            assert!(
                client.fetch_day("2024-02-29", false).await.is_err(),
                "{fault}"
            );
            first.assert_hits(2);
        }
    }

    #[tokio::test]
    async fn historical_v2_falls_back_when_unavailable_or_capped() {
        for status in [403, 404, 200] {
            let server = MockServer::start();
            server.mock(|when,then| {
                when.path("/llm/spend/logs/v2");
                then.status(status).json_body(json!({"data":[],"page":1,"page_size":1000,"total":10000,"total_pages":10,"total_is_capped":true}));
            });
            let fallback = server.mock(|when, then| {
                when.path("/llm/spend/logs")
                    .query_param("summarize", "false");
                then.status(200).json_body(rows(0, 1));
            });
            assert_eq!(
                client(&server)
                    .fetch_day("2024-02-29", false)
                    .await
                    .unwrap()
                    .models[0]
                    .spend,
                0.5
            );
            fallback.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn historical_cache_reuses_multiple_days_and_force_fetches_corrections() {
        let server = MockServer::start();
        let fallback = server.mock(|when, then| {
            when.path("/llm/spend/logs");
            then.status(200).json_body(json!([]));
        });
        let client = client(&server);
        client.fetch_day("2024-02-28", false).await.unwrap();
        client.fetch_day("2024-02-29", false).await.unwrap();
        client.fetch_day("2024-02-28", false).await.unwrap();
        fallback.assert_hits(2);
        client.fetch_day("2024-02-28", true).await.unwrap();
        fallback.assert_hits(3);
        client.fetch_day("2024-02-29", false).await.unwrap();
        fallback.assert_hits(3);
    }

    #[tokio::test]
    async fn historical_refresh_survives_visiting_another_cached_day() {
        let server = MockServer::start();
        let mut initial = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(200).json_body(page(1, 0, vec![]));
        });
        let client = client(&server);
        client.fetch_day("2024-02-28", false).await.unwrap();
        client.fetch_day("2024-02-29", false).await.unwrap();
        initial.delete();
        let corrected = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(200)
                .delay(std::time::Duration::from_millis(100))
                .json_body(page(1, 1, rows(0, 1)));
        });
        let cloned = client.clone();
        let refresh = tokio::spawn(async move { cloned.fetch_day("2024-02-29", true).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while corrected.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        client.fetch_day("2024-02-28", false).await.unwrap();
        assert_eq!(refresh.await.unwrap().unwrap().models[0].spend, 0.5);
        assert_eq!(
            client
                .fetch_day("2024-02-29", false)
                .await
                .unwrap()
                .models
                .len(),
            1
        );
        corrected.assert_hits(1);
    }

    #[tokio::test]
    async fn historical_truncated_legacy_fallback_is_never_published_or_cached() {
        let server = MockServer::start();
        server.mock(|when,then| {
            when.path("/llm/spend/logs/v2");
            then.status(200).json_body(json!({"data":[],"page":1,"page_size":1000,"total":10000,"total_pages":10,"total_is_capped":true}));
        });
        let legacy = server.mock(|when, then| {
            when.path("/llm/spend/logs");
            then.status(200)
                .header("x-litellm-spend-logs-truncated", "true")
                .json_body(rows(0, 1));
        });
        let client = client(&server);
        for _ in 0..2 {
            assert!(client.fetch_day("2024-02-29", false).await.is_err());
        }
        legacy.assert_hits(2);
    }

    #[tokio::test]
    async fn historical_force_correction_wins_over_an_older_same_day_response() {
        let server = MockServer::start();
        let mut old = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(200)
                .delay(std::time::Duration::from_millis(300))
                .json_body(page(1, 1, rows(0, 1)));
        });
        let client = client(&server);
        let cloned = client.clone();
        let pending = tokio::spawn(async move { cloned.fetch_day("2024-02-29", false).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while old.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        old.delete();
        let corrected = server.mock(|when, then| {
            when.path("/llm/spend/logs/v2");
            then.status(200).json_body(page(1, 2, rows(0, 2)));
        });
        assert_eq!(
            client.fetch_day("2024-02-29", true).await.unwrap().models[0].spend,
            1.0
        );
        pending.await.unwrap().unwrap();
        assert_eq!(
            client.fetch_day("2024-02-29", false).await.unwrap().models[0].spend,
            1.0
        );
        corrected.assert_hits(1);
    }

    #[tokio::test]
    async fn historical_cache_expires_evicts_and_isolates_connections() {
        let server = MockServer::start();
        let fallback = server.mock(|when, then| {
            when.path("/llm/spend/logs");
            then.status(200).json_body(json!([]));
        });
        let client = client(&server);
        client.fetch_day("2024-02-01", false).await.unwrap();
        client.daily.lock().await.cache.front_mut().unwrap().fetched -=
            std::time::Duration::from_secs(120);
        client.fetch_day("2024-02-01", false).await.unwrap();
        fallback.assert_hits(1); // Plus d'une minute reste frais pour une journée terminée.
        client.daily.lock().await.cache.front_mut().unwrap().fetched -= PAST_TTL;
        client.fetch_day("2024-02-01", false).await.unwrap();
        fallback.assert_hits(2);
        let first = NaiveDate::from_ymd_opt(2024, 2, 1).unwrap();
        for offset in 1..=30 {
            client
                .fetch_day(&(first + Duration::days(offset)).to_string(), false)
                .await
                .unwrap();
        }
        client.fetch_day("2024-02-02", false).await.unwrap();
        fallback.assert_hits(32);
        client.fetch_day("2024-02-01", false).await.unwrap();
        fallback.assert_hits(33); // L'entrée la plus ancienne a été évincée.
        let mut same =
            Client::new(&format!("{}/llm", server.base_url()), "sk-test".into()).unwrap();
        same.reuse_daily_cache(&client);
        same.fetch_day("2024-02-01", false).await.unwrap();
        fallback.assert_hits(33);
        let mut other_key =
            Client::new(&format!("{}/llm", server.base_url()), "other-key".into()).unwrap();
        other_key.reuse_daily_cache(&client);
        other_key.fetch_day("2024-02-01", false).await.unwrap();
        fallback.assert_hits(34);
        let other_server = MockServer::start();
        let other_mock = other_server.mock(|when, then| {
            when.path("/llm/spend/logs");
            then.status(200).json_body(json!([]));
        });
        let mut other_url = Client::new(
            &format!("{}/llm", other_server.base_url()),
            "sk-test".into(),
        )
        .unwrap();
        other_url.reuse_daily_cache(&client);
        other_url.fetch_day("2024-02-01", false).await.unwrap();
        other_mock.assert_hits(1);
    }

    #[test]
    fn historical_exact_range_respects_offsets_and_dst() {
        use super::super::tests::{d, ShiftZone};
        for (day, hour, before, after, start, end) in [
            (
                "2024-03-31",
                1,
                3600,
                7200,
                "2024-03-30T23:00:00Z",
                "2024-03-31T22:00:00Z",
            ),
            (
                "2024-10-27",
                1,
                7200,
                3600,
                "2024-10-26T22:00:00Z",
                "2024-10-27T23:00:00Z",
            ),
            (
                "2024-03-31",
                0,
                0,
                3600,
                "2024-03-30T00:00:00Z",
                "2024-04-02T00:00:00Z",
            ),
            (
                "2024-10-27",
                0,
                3600,
                0,
                "2024-10-26T23:00:00Z",
                "2024-10-28T00:00:00Z",
            ),
        ] {
            let zone = ShiftZone {
                at: d(day).and_hms_opt(hour, 0, 0).unwrap(),
                before: chrono::FixedOffset::east_opt(before).unwrap(),
                after: chrono::FixedOffset::east_opt(after).unwrap(),
            };
            assert_eq!(
                exact_range(d(day), &zone).unwrap(),
                (start.parse().unwrap(), end.parse().unwrap())
            );
        }
        let west = chrono::FixedOffset::west_opt(7 * 3600).unwrap();
        assert_eq!(
            exact_range(d("2024-02-29"), &west).unwrap(),
            (
                "2024-02-29T07:00:00Z".parse().unwrap(),
                "2024-03-01T07:00:00Z".parse().unwrap()
            )
        );
    }

    #[tokio::test]
    #[ignore]
    async fn historical_speed_live() {
        let client = Client::new(
            &std::env::var("LITELLM_BASE_URL").unwrap(),
            std::env::var("OPENAI_API_KEY").unwrap(),
        )
        .unwrap();
        let day = Local::now().date_naive().pred_opt().unwrap();
        let date = day.to_string();
        let started = std::time::Instant::now();
        let fast = client.fetch_day(&date, true).await.unwrap();
        let first_load = started.elapsed();
        let started = std::time::Instant::now();
        assert_eq!(client.fetch_day(&date, false).await.unwrap(), fast);
        let revisit = started.elapsed();
        let (start, end) = day_range(day, &Local).unwrap();
        let started = std::time::Instant::now();
        let rows: Vec<LogRow> = client
            .get(&format!(
                "/spend/logs?summarize=false&start_date={start}&end_date={end}"
            ))
            .await
            .unwrap();
        let legacy = aggregate_today(&rows, day, &Local);
        assert_eq!(fast.models.len(), legacy.len());
        for (actual, expected) in fast.models.iter().zip(&legacy) {
            assert_eq!(
                (
                    &actual.model,
                    actual.requests,
                    actual.prompt_tokens,
                    actual.completion_tokens,
                    actual.total_tokens
                ),
                (
                    &expected.model,
                    expected.requests,
                    expected.prompt_tokens,
                    expected.completion_tokens,
                    expected.total_tokens
                )
            );
            assert!((actual.spend - expected.spend).abs() < 1e-8);
        }
        println!("historical day={date} models={} requests={} v2={first_load:?} revisit={revisit:?} legacy={:?}",fast.models.len(),fast.models.iter().map(|m|m.requests).sum::<u64>(),started.elapsed());
    }
}

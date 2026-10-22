//! Source analytique légère, avec repli lorsque le serveur ne la permet pas.
use super::{domain::normalize_model, Client};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shared::ModelUsage;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Access {
    request_id: u64,
    user_id: Option<Option<String>>,
    retry_after: Option<Instant>,
}

#[derive(Clone, Default, Deserialize)]
struct Metrics {
    spend: f64,
    api_requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}
impl Metrics {
    fn valid(&self) -> bool {
        self.spend.is_finite() && self.spend >= 0.0
    }
    fn adjust(&mut self, other: &Self, add: bool) -> Option<()> {
        self.spend += if add { other.spend } else { -other.spend };
        let operation = |a: u64, b: u64| {
            if add {
                a.checked_add(b)
            } else {
                a.checked_sub(b)
            }
        };
        self.api_requests = operation(self.api_requests, other.api_requests)?;
        self.prompt_tokens = operation(self.prompt_tokens, other.prompt_tokens)?;
        self.completion_tokens = operation(self.completion_tokens, other.completion_tokens)?;
        self.total_tokens = operation(self.total_tokens, other.total_tokens)?;
        (self.spend.is_finite() && self.spend >= -1e-8).then_some(())
    }
    fn matches(&self, other: &Self) -> bool {
        (self.spend - other.spend).abs() < 1e-8
            && self.api_requests == other.api_requests
            && self.prompt_tokens == other.prompt_tokens
            && self.completion_tokens == other.completion_tokens
            && self.total_tokens == other.total_tokens
    }
}
#[derive(Deserialize)]
struct Metric {
    metrics: Metrics,
}
#[derive(Deserialize)]
struct Breakdown {
    models: HashMap<String, Metric>,
}
#[derive(Deserialize)]
struct Day {
    date: String,
    metrics: Metrics,
    breakdown: Breakdown,
}
#[derive(Deserialize)]
struct Metadata {
    has_more: bool,
    total_pages: u64,
}
#[derive(Deserialize)]
struct Response {
    results: Vec<Day>,
    metadata: Metadata,
}

impl Client {
    pub(super) async fn fetch_analytics_in_zone(
        &self,
        day: NaiveDate,
        zone: &impl TimeZone,
        force: bool,
    ) -> Option<Vec<ModelUsage>> {
        let (request_id, user_id) = {
            let mut access = self.analytics.lock().await;
            if !force
                && access
                    .retry_after
                    .is_some_and(|retry| Instant::now() < retry)
            {
                return None;
            }
            if access.user_id.is_none() {
                #[derive(Deserialize)]
                struct Info {
                    info: User,
                }
                #[derive(Deserialize)]
                struct User {
                    user_id: Option<String>,
                }
                match tokio::time::timeout(Duration::from_secs(3), self.get::<Info>("/key/info"))
                    .await
                {
                    Ok(Ok(info)) => access.user_id = Some(info.info.user_id),
                    _ => {
                        access.retry_after = Some(Instant::now() + Duration::from_secs(900));
                        return None;
                    }
                }
            }
            access.request_id += 1;
            (access.request_id, access.user_id.clone().flatten())
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            self.analytics_day(day, zone, user_id.as_deref()),
        )
        .await
        .ok()
        .flatten();
        let mut access = self.analytics.lock().await;
        if access.request_id == request_id {
            access.retry_after = if result.is_some() {
                None
            } else {
                Some(Instant::now() + Duration::from_secs(900))
            };
        }
        result
    }

    async fn analytics_day(
        &self,
        day: NaiveDate,
        zone: &impl TimeZone,
        user_id: Option<&str>,
    ) -> Option<Vec<ModelUsage>> {
        let next = day.succ_opt()?;
        let midnight = day.and_hms_opt(0, 0, 0)?;
        let next_midnight = next.and_hms_opt(0, 0, 0)?;
        let super::calendar::DayBounds { start, end, .. } = super::calendar::day_bounds(day, zone)?;
        let utc_start = midnight.and_utc();
        let utc_end = next_midnight.and_utc();
        let mut url =
            reqwest::Url::parse(&format!("{}/user/daily/activity/aggregated", self.base_url))
                .ok()?;
        url.query_pairs_mut().extend_pairs([
            ("start_date", day.to_string()),
            ("end_date", day.to_string()),
            (
                "api_key",
                format!("{:x}", Sha256::digest(self.api_key.as_bytes())),
            ),
        ]);
        if let Some(user_id) = user_id {
            url.query_pairs_mut().append_pair("user_id", user_id);
        }
        let path = format!("/user/daily/activity/aggregated?{}", url.query()?);
        let response: Response = tokio::time::timeout(Duration::from_secs(3), self.get(&path))
            .await
            .ok()?
            .ok()?;
        if response.metadata.has_more
            || response.metadata.total_pages > 1
            || response.results.len() != 1
        {
            return None;
        }
        let row = response.results.into_iter().next()?;
        if row.date != day.to_string() || !row.metrics.valid() {
            return None;
        }
        let mut models = HashMap::<String, Metrics>::new();
        let mut total = Metrics::default();
        for (name, model) in row.breakdown.models {
            if !model.metrics.valid() {
                return None;
            }
            total.adjust(&model.metrics, true)?;
            let name = normalize_model(&name)?;
            models
                .entry(name.into())
                .or_default()
                .adjust(&model.metrics, true)?;
        }
        if !total.matches(&row.metrics) {
            return None;
        }
        let (first, last) = tokio::join!(
            self.analytics_boundary(start.min(utc_start), start.max(utc_start)),
            self.analytics_boundary(end.min(utc_end), end.max(utc_end))
        );
        for (correction, add) in [(first?, start < utc_start), (last?, end > utc_end)] {
            for (name, metrics) in correction {
                models.entry(name).or_default().adjust(&metrics, add)?;
            }
        }
        let mut result: Vec<_> = models
            .into_iter()
            .map(|(model, m)| ModelUsage {
                model,
                spend: m.spend.max(0.0),
                requests: m.api_requests,
                prompt_tokens: m.prompt_tokens,
                completion_tokens: m.completion_tokens,
                total_tokens: m.total_tokens,
            })
            .collect();
        result.sort_by(|a, b| {
            b.total_tokens
                .cmp(&a.total_tokens)
                .then(a.model.cmp(&b.model))
        });
        Some(result)
    }

    async fn analytics_boundary(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Option<HashMap<String, Metrics>> {
        let mut models = HashMap::<String, Metrics>::new();
        if start == end {
            return Some(models);
        }
        for row in self.fetch_range_usage_pages(start, end).await.ok()?? {
            if row.start_time < start || row.start_time >= end {
                continue;
            }
            let Some(name) = normalize_model(&row.model) else {
                continue;
            };
            let metrics = Metrics {
                spend: row.spend?,
                api_requests: 1,
                prompt_tokens: row.prompt_tokens?,
                completion_tokens: row.completion_tokens?,
                total_tokens: row.total_tokens?,
            };
            if !metrics.valid() {
                return None;
            }
            models
                .entry(name.into())
                .or_default()
                .adjust(&metrics, true)?;
        }
        Some(models)
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use httpmock::prelude::*;
    use serde_json::{json, Value};

    fn metrics(spend: f64, requests: u64, input: u64, output: u64) -> Value {
        json!({"spend":spend,"api_requests":requests,"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output})
    }
    fn response(day: &str) -> Value {
        json!({"results":[{"date":day,"metrics":metrics(5.0,2,100,20),"breakdown":{"models":{
            "openai/m":{"metrics":metrics(3.0,1,60,10)},"m":{"metrics":metrics(2.0,1,40,10)}
        }}}],"metadata":{"page":1,"total_pages":1,"has_more":false}})
    }
    fn info(server: &MockServer) {
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(200)
                .json_body(json!({"info":{"user_id":"test-user","spend":5.0}}));
        });
    }

    #[tokio::test]
    async fn preferred_daily_source_scopes_key_and_normalizes_models() {
        let server = MockServer::start();
        info(&server);
        let aggregate = server.mock(|when, then| {
            when.path("/user/daily/activity/aggregated")
                .header("authorization", "Bearer sk-test")
                .query_param(
                    "api_key",
                    "f3abf2a6cc4f00987743db5f544ba345b4899ae31f326d8ee9c4816de153c9e0",
                )
                .query_param("user_id", "test-user")
                .query_param("start_date", "2024-02-29")
                .query_param("end_date", "2024-02-29");
            then.status(200).json_body(response("2024-02-29"));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let result = client
            .fetch_analytics_in_zone(day, &Utc, false)
            .await
            .unwrap();
        assert_eq!(
            result,
            vec![shared::ModelUsage {
                model: "m".into(),
                spend: 5.0,
                requests: 2,
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120
            }]
        );
        aggregate.assert_hits(1);
    }

    #[tokio::test]
    async fn preferred_daily_source_is_used_by_cached_full_day_loading() {
        let server = MockServer::start();
        info(&server);
        let aggregate = server.mock(|when, then| {
            when.path("/user/daily/activity/aggregated");
            then.status(200).json_body(response("2024-02-29"));
        });
        let boundaries=server.mock(|when,then| {
            when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        assert_eq!(
            client.fetch_day("2024-02-29", false).await.unwrap().models[0].total_tokens,
            120
        );
        let queries = boundaries.hits();
        assert_eq!(
            client.fetch_day("2024-02-29", false).await.unwrap().models[0].spend,
            5.0
        );
        assert_eq!(
            client
                .fetch_day_costs("2024-02-29", false)
                .await
                .unwrap()
                .models[0]
                .spend,
            5.0
        );
        aggregate.assert_hits(1);
        boundaries.assert_hits(queries);
    }

    #[tokio::test]
    async fn preferred_daily_slow_boundary_does_not_delay_fallback_indefinitely() {
        let server = MockServer::start();
        info(&server);
        server.mock(|when, then| {
            when.path("/user/daily/activity/aggregated");
            then.status(200).json_body(response("2024-02-29"));
        });
        server.mock(|when,then| {
            when.path("/spend/logs/v2");then.status(200).delay(std::time::Duration::from_secs(6)).json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let zone = chrono::FixedOffset::east_opt(7200).unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(5500),
            client.fetch_analytics_in_zone(day, &zone, false),
        )
        .await;
        assert!(
            matches!(outcome, Ok(None)),
            "slow corrections must yield to existing endpoint"
        );
    }

    #[tokio::test]
    async fn preferred_daily_latest_attempt_controls_retry_after_overlap() {
        for latest_succeeds in [true, false] {
            let server = MockServer::start();
            info(&server);
            let old = server.mock(|when, then| {
                when.path("/user/daily/activity/aggregated")
                    .query_param("start_date", "2024-02-28");
                let then = then.delay(std::time::Duration::from_secs(1));
                if latest_succeeds {
                    then.status(403);
                } else {
                    then.status(200).json_body(response("2024-02-28"));
                }
            });
            server.mock(|when, then| {
                when.path("/user/daily/activity/aggregated")
                    .query_param("start_date", "2024-02-29");
                if latest_succeeds {
                    then.status(200).json_body(response("2024-02-29"));
                } else {
                    then.status(403);
                }
            });
            let next = server.mock(|when, then| {
                when.path("/user/daily/activity/aggregated")
                    .query_param("start_date", "2024-03-01");
                then.status(200).json_body(response("2024-03-01"));
            });
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            let cloned = client.clone();
            let pending = tokio::spawn(async move {
                cloned
                    .fetch_analytics_in_zone(
                        NaiveDate::from_ymd_opt(2024, 2, 28).unwrap(),
                        &Utc,
                        false,
                    )
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while old.hits_async().await == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let latest = client
                .fetch_analytics_in_zone(NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(), &Utc, true)
                .await;
            assert_eq!(latest.is_some(), latest_succeeds);
            assert!(
                !pending.is_finished(),
                "the earlier attempt must still be pending"
            );
            assert_eq!(pending.await.unwrap().is_some(), !latest_succeeds);
            let result = client
                .fetch_analytics_in_zone(NaiveDate::from_ymd_opt(2024, 3, 1).unwrap(), &Utc, false)
                .await;
            assert_eq!(
                result.is_some(),
                latest_succeeds,
                "an older outcome must not override the forced retry"
            );
            if let Some(models) = result {
                assert_eq!(models[0].total_tokens, 120);
            }
            next.assert_hits(usize::from(latest_succeeds));
        }
    }

    #[tokio::test]
    async fn preferred_daily_denial_falls_back_and_force_retries() {
        for status in [403, 404, 500] {
            let server = MockServer::start();
            info(&server);
            let unavailable = server.mock(|when, then| {
                when.path("/user/daily/activity/aggregated");
                then.status(status);
            });
            let fallback=server.mock(|when,then| {
                when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false}));
            });
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            client.fetch_day("2024-02-28", false).await.unwrap();
            client.fetch_day("2024-02-29", false).await.unwrap();
            unavailable.assert_hits(1);
            fallback.assert_hits(2);
            client.fetch_day("2024-02-29", true).await.unwrap();
            unavailable.assert_hits(2);
            fallback.assert_hits(3);
        }
    }

    #[tokio::test]
    async fn preferred_daily_rejects_partial_missing_or_inconsistent_metrics() {
        for fault in [
            "missing_tokens",
            "wrong_date",
            "partial",
            "inconsistent",
            "negative",
        ] {
            let server = MockServer::start();
            info(&server);
            let mut body = response("2024-02-29");
            match fault {
                "missing_tokens" => {
                    body["results"][0]["breakdown"]["models"]["m"]["metrics"]
                        .as_object_mut()
                        .unwrap()
                        .remove("prompt_tokens");
                }
                "wrong_date" => body["results"][0]["date"] = json!("2024-02-28"),
                "partial" => body["metadata"]["has_more"] = json!(true),
                "negative" => body["results"][0]["metrics"]["spend"] = json!(-1),
                _ => body["results"][0]["metrics"]["total_tokens"] = json!(999),
            }
            server.mock(|when, then| {
                when.path("/user/daily/activity/aggregated");
                then.status(200).json_body(body);
            });
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            assert!(
                client
                    .fetch_analytics_in_zone(
                        NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
                        &Utc,
                        false
                    )
                    .await
                    .is_none(),
                "{fault}"
            );
        }
    }

    #[tokio::test]
    async fn preferred_daily_rejects_missing_boundary_metrics() {
        for missing in [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "spend",
        ] {
            let server = MockServer::start();
            info(&server);
            server.mock(|when, then| {
                when.path("/user/daily/activity/aggregated");
                then.status(200).json_body(response("2024-02-29"));
            });
            server.mock(|when, then| {
                when.path("/spend/logs/v2").query_param("start_date", "2024-02-28 22:00:00");
                then.status(200).json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false}));
            });
            let mut row = json!({"request_id":"excluded-from-local-day","model":"m","spend":1.0,
                "prompt_tokens":10,"completion_tokens":1,"total_tokens":11,"startTime":"2024-02-29T22:00:00Z"});
            row.as_object_mut().unwrap().remove(missing);
            server.mock(|when, then| {
                when.path("/spend/logs/v2").query_param("start_date", "2024-02-29 22:00:00");
                then.status(200).json_body(json!({"data":[row],"total":1,"page":1,"page_size":1000,"total_pages":1,"total_is_capped":false}));
            });
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            let result = client
                .fetch_analytics_in_zone(
                    NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
                    &chrono::FixedOffset::east_opt(7200).unwrap(),
                    false,
                )
                .await;
            assert!(
                result.is_none(),
                "missing {missing} must trigger fallback, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn preferred_daily_corrects_costs_and_tokens_at_local_midnight() {
        let server = MockServer::start();
        info(&server);
        server.mock(|when, then| {
            when.path("/user/daily/activity/aggregated");
            then.status(200).json_body(response("2024-02-29"));
        });
        for (start, end, spend, input, output) in [
            ("2024-02-28 22:00:00", "2024-02-29 00:00:00", 3.0, 30, 3),
            ("2024-02-29 22:00:00", "2024-03-01 00:00:00", 1.0, 10, 1),
        ] {
            server.mock(|when,then| {
                when.path("/spend/logs/v2").query_param("start_date",start).query_param("end_date",end);
                then.status(200).json_body(json!({"data":[
                    {"request_id":"included","model":"openai/m","spend":spend,"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output,"startTime":format!("{}Z",start.replace(' ',"T"))},
                    {"request_id":"excluded","model":"m","spend":99,"prompt_tokens":999,"total_tokens":999,"startTime":format!("{}Z",end.replace(' ',"T"))}
                ],"total":2,"page":1,"page_size":1000,"total_pages":1,"total_is_capped":false}));
            });
        }
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let data = client
            .fetch_analytics_in_zone(
                NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
                &chrono::FixedOffset::east_opt(7200).unwrap(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            data,
            vec![shared::ModelUsage {
                model: "m".into(),
                spend: 7.0,
                requests: 2,
                prompt_tokens: 120,
                completion_tokens: 22,
                total_tokens: 142
            }]
        );
    }
}

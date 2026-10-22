//! Coûts locaux progressifs : un résumé et des corrections de frontières partagées.
use super::{
    calendar::{day_bounds, DayBounds},
    domain::normalize_model,
    Client,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shared::{DailyCosts, ModelCost};
use std::collections::HashMap;

type Interval = (DateTime<Utc>, DateTime<Utc>);

#[derive(Deserialize)]
struct Summary {
    #[serde(rename = "startTime")]
    date: String,
    spend: f64,
    models: HashMap<String, f64>,
}

struct Pending {
    bounds: DayBounds,
    id: u64,
    models: HashMap<String, f64>,
    corrections: Vec<(Interval, f64)>,
}

impl Client {
    pub async fn fetch_local_history(
        &self,
        today: NaiveDate,
        start: NaiveDate,
        force: bool,
        publish: impl Fn(DailyCosts) + Send + Sync,
    ) -> Result<(), String> {
        self.fetch_local_history_in_zone(today, start, force, &Local, publish)
            .await
    }

    async fn fetch_local_history_in_zone(
        &self,
        today: NaiveDate,
        start: NaiveDate,
        force: bool,
        zone: &impl TimeZone,
        publish: impl Fn(DailyCosts) + Send + Sync,
    ) -> Result<(), String> {
        let mut pending = vec![];
        let mut error = None;
        for day in start
            .iter_days()
            .take_while(|d| *d <= today)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let Some(bounds) = day_bounds(day, zone) else {
                error = Some("Frontière locale indisponible".to_owned());
                continue;
            };
            if !force {
                let cached =
                    if let Some(usage) = self.cached_day_in_zone(&day.to_string(), zone).await {
                        Some(DailyCosts {
                            date: usage.date,
                            models: usage
                                .models
                                .into_iter()
                                .map(|m| ModelCost {
                                    model: m.model,
                                    spend: m.spend,
                                })
                                .collect(),
                        })
                    } else {
                        self.cached_costs(bounds).await
                    };
                if let Some(value) = cached {
                    publish(value);
                    continue;
                }
            }
            let id = self.reserve_costs(bounds).await;
            let midnight = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
            let next = day
                .succ_opt()
                .ok_or("Date hors limites")?
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc();
            let corrections = [
                (
                    bounds.start,
                    midnight,
                    if bounds.start < midnight { 1.0 } else { -1.0 },
                ),
                (bounds.end, next, if bounds.end > next { 1.0 } else { -1.0 }),
            ]
            .into_iter()
            .filter(|(a, b, _)| a != b)
            .map(|(a, b, sign)| ((a.min(b), a.max(b)), sign))
            .collect();
            pending.push(Pending {
                bounds,
                id,
                models: HashMap::new(),
                corrections,
            });
        }
        if pending.is_empty() {
            return error.map_or(Ok(()), Err);
        }
        let hash = format!("{:x}", Sha256::digest(self.api_key.as_bytes()));
        let end = today.succ_opt().ok_or("Date hors limites")?;
        let rows: Vec<Summary> = self
            .get(&format!(
                "/spend/logs?summarize=true&start_date={start}&end_date={end}&api_key={hash}"
            ))
            .await
            .map_err(String::from)?;
        for row in rows {
            let date = super::parse_date(&row.date).ok_or("Date du résumé invalide")?;
            let Some(day) = pending.iter_mut().find(|p| p.bounds.date == date) else {
                continue;
            };
            if !row.spend.is_finite()
                || row.spend < 0.0
                || row.models.values().any(|v| !v.is_finite() || *v < 0.0)
                || (row.models.values().sum::<f64>() - row.spend).abs() > 1e-8
            {
                return Err("Résumé des coûts incomplet".into());
            }
            for (name, spend) in row.models {
                let name = normalize_model(&name).ok_or("Modèle du résumé invalide")?;
                *day.models.entry(name.into()).or_default() += spend;
            }
        }
        let mut intervals = vec![];
        for day in &pending {
            for (interval, _) in &day.corrections {
                if !intervals.contains(interval) {
                    intervals.push(*interval);
                }
            }
        }
        let mut completed = HashMap::<Interval, Result<Vec<ModelCost>, String>>::new();
        let mut requests =
            stream::iter(intervals)
                .map(|interval| async move {
                    (interval, self.boundary_costs(interval.0, interval.1).await)
                })
                .buffer_unordered(3);
        loop {
            let mut index = 0;
            while index < pending.len() {
                if pending[index]
                    .corrections
                    .iter()
                    .any(|(interval, _)| !completed.contains_key(interval))
                {
                    index += 1;
                    continue;
                }
                let mut day = pending.remove(index);
                let mut valid = true;
                for (interval, sign) in day.corrections {
                    match &completed[&interval] {
                        Ok(costs) => {
                            for model in costs {
                                *day.models.entry(model.model.clone()).or_default() +=
                                    sign * model.spend;
                            }
                        }
                        Err(message) => {
                            error = Some(message.clone());
                            valid = false;
                        }
                    }
                }
                if day.models.values().any(|v| !v.is_finite() || *v < -1e-8) {
                    valid = false;
                    error = Some("Coûts locaux incohérents".into());
                }
                if !valid || day_bounds(day.bounds.date, zone) != Some(day.bounds) {
                    continue;
                }
                let mut models: Vec<_> = day
                    .models
                    .into_iter()
                    .map(|(model, spend)| ModelCost {
                        model,
                        spend: spend.max(0.0),
                    })
                    .collect();
                models.sort_by(|a, b| b.spend.total_cmp(&a.spend).then(a.model.cmp(&b.model)));
                let value = DailyCosts {
                    date: day.bounds.date.to_string(),
                    models,
                };
                if self.store_costs(day.bounds, day.id, &value).await {
                    publish(value);
                }
            }
            match requests.next().await {
                Some((interval, result)) => {
                    completed.insert(interval, result);
                }
                None => break,
            }
        }
        error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use crate::litellm::{tests::d, Client};
    use chrono::{FixedOffset, Utc};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn local_history_reuses_shared_boundaries_and_publishes_recent_days_first() {
        let server = MockServer::start();
        let summary = server.mock(|when, then| {
            when.path("/spend/logs").query_param(
                "api_key",
                "f3abf2a6cc4f00987743db5f544ba345b4899ae31f326d8ee9c4816de153c9e0",
            );
            then.status(200).json_body(json!([
                {"startTime":"2024-02-27","spend":10,"models":{"m":10}},
                {"startTime":"2024-02-28","spend":10,"models":{"m":10}},
                {"startTime":"2024-02-29","spend":10,"models":{"m":10}}
            ]));
        });
        let boundaries: Vec<_> = [("2024-02-26",3), ("2024-02-27",1), ("2024-02-28",3), ("2024-02-29",1)].into_iter().map(|(date,spend)| {
            server.mock(|when, then| {
                when.path("/spend/logs/v2").query_param("start_date",format!("{date} 22:00:00"));
                then.status(200).delay(std::time::Duration::from_millis(if date=="2024-02-26" {200} else {0}))
                    .json_body(json!({"data":[{"request_id":"r","model":"m","spend":spend,"startTime":format!("{date}T22:00:00Z")}],"total":1,"page":1,"page_size":1000,"total_pages":1}));
            })
        }).collect();
        let published = Arc::new(Mutex::new(vec![]));
        let output = published.clone();
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        client
            .fetch_local_history_in_zone(
                d("2024-02-29"),
                d("2024-02-27"),
                false,
                &FixedOffset::east_opt(7200).unwrap(),
                move |cost| output.lock().unwrap().push(cost),
            )
            .await
            .unwrap();
        let result = published.lock().unwrap();
        assert_eq!(result[0].date, "2024-02-29");
        assert_eq!(result[0].models[0].spend, 12.0);
        assert_eq!(
            result
                .iter()
                .find(|c| c.date == "2024-02-28")
                .unwrap()
                .models[0]
                .spend,
            8.0
        );
        summary.assert_hits(1);
        for boundary in boundaries {
            boundary.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn local_history_covers_west_dst_and_missing_midnight() {
        use crate::litellm::tests::ShiftZone;
        for (date, before, after, hour, first_start, first_end, last_start, last_end, expected) in [
            (
                "2024-02-29",
                -7200,
                -7200,
                1,
                "2024-02-29 00:00:00",
                "2024-02-29 02:00:00",
                "2024-03-01 00:00:00",
                "2024-03-01 02:00:00",
                8.0,
            ),
            (
                "2024-03-31",
                3600,
                7200,
                1,
                "2024-03-30 23:00:00",
                "2024-03-31 00:00:00",
                "2024-03-31 22:00:00",
                "2024-04-01 00:00:00",
                12.0,
            ),
            (
                "2024-10-27",
                7200,
                3600,
                1,
                "2024-10-26 22:00:00",
                "2024-10-27 00:00:00",
                "2024-10-27 23:00:00",
                "2024-10-28 00:00:00",
                12.0,
            ),
        ] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/spend/logs");
                then.status(200)
                    .json_body(json!([{"startTime":date,"spend":10,"models":{"m":10}}]));
            });
            let boundaries:Vec<_>=[(first_start,first_end,3),(last_start,last_end,1)].into_iter().map(|(start,end,spend)|server.mock(|when,then| {
                when.path("/spend/logs/v2").query_param("start_date",start).query_param("end_date",end);
                then.status(200).json_body(json!({"data":[{"request_id":"r","model":"m","spend":spend,"startTime":format!("{}Z",start.replace(' ',"T"))}],"total":1,"page":1,"page_size":1000,"total_pages":1}));
            })).collect();
            let zone = ShiftZone {
                at: d(date).and_hms_opt(hour, 0, 0).unwrap(),
                before: FixedOffset::east_opt(before).unwrap(),
                after: FixedOffset::east_opt(after).unwrap(),
            };
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            let values = Mutex::new(vec![]);
            client
                .fetch_local_history_in_zone(d(date), d(date), false, &zone, |v| {
                    values.lock().unwrap().push(v)
                })
                .await
                .unwrap();
            assert_eq!(values.lock().unwrap()[0].models[0].spend, expected);
            for boundary in boundaries {
                boundary.assert_hits(1);
            }
        }
        let server = MockServer::start();
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(500);
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let zone = ShiftZone {
            at: d("2024-03-31").and_hms_opt(0, 0, 0).unwrap(),
            before: FixedOffset::east_opt(0).unwrap(),
            after: FixedOffset::east_opt(3600).unwrap(),
        };
        assert!(client
            .fetch_local_history_in_zone(
                d("2024-03-31"),
                d("2024-03-31"),
                false,
                &zone,
                |_| panic!("no guessed local date")
            )
            .await
            .is_err());
        any.assert_hits(0);
    }

    #[tokio::test]
    async fn local_history_limits_active_boundary_intervals() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(json!([]));
        });
        let requests: Vec<_> = ["2024-02-28", "2024-02-29", "2024-02-27", "2024-02-26"]
            .into_iter()
            .map(|date| {
                server.mock(|when, then| {
                    when.path("/spend/logs/v2")
                        .query_param("start_date", format!("{date} 22:00:00"));
                    then.status(200)
                        .delay(std::time::Duration::from_millis(500))
                        .json_body(
                            json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}),
                        );
                })
            })
            .collect();
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let pending = tokio::spawn(async move {
            client
                .fetch_local_history_in_zone(
                    d("2024-02-29"),
                    d("2024-02-27"),
                    false,
                    &FixedOffset::east_opt(7200).unwrap(),
                    |_| {},
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_millis(400), async {
            while requests[..3].iter().any(|r| r.hits() == 0) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        requests[3].assert_hits(0);
        pending.await.unwrap().unwrap();
        for request in requests {
            request.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn local_history_rejects_inconsistent_summary_models() {
        for models in [json!({"m":9}), json!({"m":-1}), json!({})] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/spend/logs");
                then.status(200)
                    .json_body(json!([{"startTime":"2024-02-29","spend":10,"models":models}]));
            });
            let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
            assert!(client
                .fetch_local_history_in_zone(
                    d("2024-02-29"),
                    d("2024-02-29"),
                    false,
                    &Utc,
                    |_| panic!("invalid totals published")
                )
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn local_history_cannot_overwrite_newer_refresh() {
        let server = MockServer::start();
        let mut old = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .delay(std::time::Duration::from_millis(250))
                .json_body(json!([{"startTime":"2024-02-29","spend":1,"models":{"m":1}}]));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let cloned = client.clone();
        let published = Arc::new(Mutex::new(vec![]));
        let output = published.clone();
        let pending = tokio::spawn(async move {
            cloned
                .fetch_local_history_in_zone(
                    d("2024-02-29"),
                    d("2024-02-29"),
                    false,
                    &Utc,
                    move |v| output.lock().unwrap().push(v),
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
        old.delete();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{"startTime":"2024-02-29","spend":2,"models":{"m":2}}]));
        });
        client
            .fetch_day_costs_cached_in_zone("2024-02-29", true, &Utc)
            .await
            .unwrap();
        pending.await.unwrap().unwrap();
        assert!(published.lock().unwrap().is_empty());
        assert_eq!(
            client
                .fetch_day_costs_cached_in_zone("2024-02-29", false, &Utc)
                .await
                .unwrap()
                .models[0]
                .spend,
            2.0
        );
    }

    #[tokio::test]
    async fn local_history_utc_reuses_cache_and_force_reloads() {
        let server = MockServer::start();
        let summary = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .json_body(json!([{ "startTime":"2024-02-29","spend":10,"models":{"m":10}}]));
        });
        let logs = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(500);
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        for force in [false, false, true] {
            client
                .fetch_local_history_in_zone(
                    d("2024-02-29"),
                    d("2024-02-29"),
                    force,
                    &Utc,
                    |cost| assert_eq!(cost.models[0].spend, 10.0),
                )
                .await
                .unwrap();
        }
        summary.assert_hits(2);
        logs.assert_hits(0);
    }

    #[tokio::test]
    async fn local_history_preserves_good_days_when_one_boundary_fails() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(json!([]));
        });
        server.mock(|when,then| {
            when.path("/spend/logs/v2").query_param("start_date","2024-02-27 22:00:00");
            then.status(200).json_body(json!({"data":[{"request_id":"r","model":"m","startTime":"2024-02-27T22:00:00Z"}],"total":1,"page":1,"page_size":1000,"total_pages":1}));
        });
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200)
                .json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let output = Mutex::new(vec![]);
        assert!(client
            .fetch_local_history_in_zone(
                d("2024-02-29"),
                d("2024-02-28"),
                false,
                &FixedOffset::east_opt(7200).unwrap(),
                |c| output.lock().unwrap().push(c)
            )
            .await
            .is_err());
        let result = output.lock().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].date, "2024-02-29");
        assert!(result[0].models.is_empty());
    }
}

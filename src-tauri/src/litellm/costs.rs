//! Coûts rapides : résumé UTC corrigé aux frontières de la journée locale.
use super::{
    aggregate_totals,
    domain::{normalize_model, DaySummary},
    Client,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use sha2::{Digest, Sha256};
use shared::{DailyCosts, ModelCost};
use std::collections::HashMap;

impl Client {
    pub async fn fetch_day_costs(&self, date: &str, force: bool) -> Result<DailyCosts, String> {
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .filter(|day| {
                date.len() == 10 && day.to_string() == date && *day <= Local::now().date_naive()
            })
            .ok_or("Date invalide ou future")?;
        if !force {
            if let Some(usage) = self.cached_day(date).await {
                return Ok(DailyCosts {
                    date: usage.date,
                    models: usage
                        .models
                        .into_iter()
                        .map(|m| ModelCost {
                            model: m.model,
                            spend: m.spend,
                        })
                        .collect(),
                });
            }
        }
        self.fetch_day_costs_in_zone(day, &Local).await
    }

    async fn fetch_day_costs_in_zone(
        &self,
        day: NaiveDate,
        zone: &impl TimeZone,
    ) -> Result<DailyCosts, String> {
        let next = day.succ_opt().ok_or("Date hors limites")?;
        let midnight = day.and_hms_opt(0, 0, 0).unwrap();
        let next_midnight = next.and_hms_opt(0, 0, 0).unwrap();
        let start = zone
            .from_local_datetime(&midnight)
            .earliest()
            .ok_or("Frontière locale indisponible")?
            .with_timezone(&Utc);
        let end = zone
            .from_local_datetime(&next_midnight)
            .earliest()
            .ok_or("Frontière locale indisponible")?
            .with_timezone(&Utc);
        let utc_start = midnight.and_utc();
        let utc_end = next_midnight.and_utc();
        let summary = async {
            let hash = format!("{:x}", Sha256::digest(self.api_key.as_bytes()));
            let rows: Vec<DaySummary> = self
                .get(&format!(
                    "/spend/logs?summarize=true&start_date={day}&end_date={next}&api_key={hash}"
                ))
                .await
                .map_err(String::from)?;
            if rows.iter().any(|row| {
                row.start_time
                    .as_deref()
                    .and_then(super::parse_date)
                    .is_none()
            }) {
                return Err("Date du résumé de consommation invalide".into());
            }
            let selected: Vec<_> = rows
                .into_iter()
                .filter(|row| row.start_time.as_deref().and_then(super::parse_date) == Some(day))
                .collect();
            Ok::<_, String>(aggregate_totals(&selected))
        };
        let (summary, start_costs, end_costs) = tokio::try_join!(
            summary,
            self.boundary_costs(start.min(utc_start), start.max(utc_start)),
            self.boundary_costs(end.min(utc_end), end.max(utc_end)),
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

    async fn boundary_costs(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ModelCost>, String> {
        if start == end {
            return Ok(vec![]);
        }
        let rows = self
            .fetch_range_pages(start, end)
            .await?
            .ok_or("Correction de la journée locale indisponible")?;
        let mut costs = HashMap::new();
        for row in rows {
            if !row
                .start_time
                .is_some_and(|time| time >= start && time < end)
            {
                continue;
            }
            if let Some(model) = normalize_model(&row.model) {
                *costs.entry(model.to_owned()).or_insert(0.0) += row.spend;
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
    use super::super::Client;
    use chrono::{NaiveDate, Utc};
    use httpmock::prelude::*;
    use serde_json::json;

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
        let date = chrono::Local::now()
            .date_naive()
            .pred_opt()
            .unwrap()
            .to_string();
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

//! Aperçu du calendrier : agrégats UTC seulement, détails locaux chargés à la sélection.
use super::Client;
use chrono::NaiveDate;
use serde::Deserialize;
use shared::HistoryDay;

impl Client {
    pub async fn fetch_local_history(
        &self,
        today: NaiveDate,
        start: NaiveDate,
        publish: impl Fn(HistoryDay) + Send + Sync,
    ) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Summary {
            #[serde(rename = "startTime")]
            date: String,
            spend: f64,
        }
        let end = today.succ_opt().ok_or("Date hors limites")?;
        let rows: Vec<Summary> = self
            .get(
                &self
                    .scoped_path(format!(
                        "/spend/logs?summarize=true&start_date={start}&end_date={end}"
                    ))
                    .await?,
            )
            .await
            .map_err(String::from)?;
        let amounts = crate::history::aggregate(
            today,
            start,
            rows.iter().map(|r| (Some(r.date.as_str()), r.spend)),
        )?;
        let first = today - chrono::Duration::days(29);
        for (index, spend) in amounts.into_iter().enumerate().rev() {
            if let Some(spend) = spend {
                publish(HistoryDay {
                    date: (first + chrono::Duration::days(index as i64)).to_string(),
                    spend: Some(spend),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::litellm::{tests::d, Client};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[tokio::test]
    #[ignore]
    async fn aggregate_heatmap_live() {
        let client = Client::new(
            &std::env::var("LITELLM_BASE_URL").unwrap(),
            std::env::var("OPENAI_API_KEY").unwrap(),
        )
        .unwrap();
        let today = chrono::Local::now().date_naive();
        let output = Mutex::new(vec![]);
        let started = std::time::Instant::now();
        client
            .fetch_local_history(today, today - chrono::Duration::days(29), |day| {
                output.lock().unwrap().push(day)
            })
            .await
            .unwrap();
        let values = output.lock().unwrap();
        assert_eq!(values.len(), 30);
        assert_eq!(values[0].date, today.to_string());
        assert!(values
            .iter()
            .all(|v| v.spend.is_some_and(|s| s.is_finite() && s >= 0.0)));
        println!(
            "aggregate_heatmap={:?} days={}",
            started.elapsed(),
            values.len()
        );
    }

    #[tokio::test]
    async fn aggregate_heatmap_needs_no_models_or_detail_queries() {
        let server = MockServer::start();
        let summary = server.mock(|when, then| {
            when.path("/spend/logs")
                .query_param("summarize", "true")
                .query_param(
                    "api_key",
                    "f3abf2a6cc4f00987743db5f544ba345b4899ae31f326d8ee9c4816de153c9e0",
                );
            then.status(200).json_body(
                json!([{"startTime":"2024-02-29","spend":7},{"startTime":"2024-03-01","spend":99}]),
            );
        });
        let details = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(500);
        });
        let client = Client::for_spend_test(&server.base_url(), "sk-test".into()).unwrap();
        let output = Mutex::new(vec![]);
        client
            .fetch_local_history(d("2024-02-29"), d("2024-02-28"), |day| {
                output.lock().unwrap().push(day)
            })
            .await
            .unwrap();
        let result = output.lock().unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].date, "2024-02-29");
        assert_eq!(result[0].spend, Some(7.0));
        assert_eq!(result[1].spend, Some(0.0));
        summary.assert_hits(1);
        details.assert_hits(0);
    }
}

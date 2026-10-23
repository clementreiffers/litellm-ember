//! Mutualise les corrections en vol et leurs lignes complètes pendant quelques secondes.
use super::{daily::UsageRow, Client};
use chrono::{DateTime, Utc};
use futures_util::{
    future::{BoxFuture, Shared},
    FutureExt,
};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

type Rows = Result<Option<Arc<Vec<UsageRow>>>, String>;
type Request = Shared<BoxFuture<'static, (Instant, Rows)>>;
const CAPACITY: usize = 60;
const TTL: Duration = Duration::from_secs(15);

struct Entry {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    created: Instant,
    request: Request,
}
#[derive(Default)]
pub(super) struct RangeState {
    entries: VecDeque<Entry>,
}

impl Client {
    pub(super) async fn shared_boundary_rows(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fresh_since: Option<Instant>,
    ) -> Rows {
        let request = {
            let mut state = self.ranges.lock().await;
            state.entries.retain(|e| match e.request.peek() {
                Some((finished, Ok(Some(_)))) => finished.elapsed() < TTL,
                Some(_) => false,
                None => e.created.elapsed() < Duration::from_secs(60),
            });
            if let Some(entry) = state.entries.iter().find(|e| {
                e.start == start
                    && e.end == end
                    && fresh_since.is_none_or(|since| e.created >= since)
            }) {
                entry.request.clone()
            } else {
                state.entries.retain(|e| e.start != start || e.end != end);
                // Ne pas capturer le Client qui possède ce cache dans le futur qu'il stocke.
                let transport =
                    Self::with_http(&self.base_url, self.api_key.clone(), self.http.clone())?;
                let request = async move {
                    let rows = transport
                        .fetch_range_usage_pages(start, end)
                        .await
                        .map(|rows| rows.map(Arc::new));
                    (Instant::now(), rows)
                }
                .boxed()
                .shared();
                state.entries.push_back(Entry {
                    start,
                    end,
                    created: Instant::now(),
                    request: request.clone(),
                });
                while state.entries.len() > CAPACITY {
                    state.entries.pop_front();
                }
                request
            }
        };
        let (_, result) = request.clone().await;
        if !matches!(result, Ok(Some(_))) {
            self.ranges
                .lock()
                .await
                .entries
                .retain(|e| !e.request.ptr_eq(&request));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    #[tokio::test]
    async fn forced_boundary_does_not_join_an_older_inflight_request() {
        let server = MockServer::start();
        let mut old = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200)
                .delay(Duration::from_millis(200))
                .json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let start = "2024-02-29T00:00:00Z".parse().unwrap();
        let end = "2024-02-29T02:00:00Z".parse().unwrap();
        let cloned = client.clone();
        let pending =
            tokio::spawn(async move { cloned.shared_boundary_rows(start, end, None).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while old.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        old.delete();
        let fresh=server.mock(|when,then|{when.path("/spend/logs/v2");then.status(200).json_body(json!({"data":[{"request_id":"new","model":"m","spend":5,"startTime":"2024-02-29T01:00:00Z"}],"total":1,"page":1,"page_size":1000,"total_pages":1}));});
        let rows = client
            .shared_boundary_rows(start, end, Some(Instant::now()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].spend, Some(5.0));
        fresh.assert_hits(1);
        pending.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shared_boundaries_discard_errors_and_bypass_completed_values_on_force() {
        let server = MockServer::start();
        let mut failed = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(503).delay(Duration::from_millis(50));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let start = "2024-02-29T00:00:00Z".parse().unwrap();
        let end = "2024-02-29T02:00:00Z".parse().unwrap();
        let (a, b) = tokio::join!(
            client.shared_boundary_rows(start, end, None),
            client.shared_boundary_rows(start, end, None)
        );
        assert!(a.is_err() && b.is_err());
        failed.assert_hits(1);
        failed.delete();
        let valid = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200)
                .json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}));
        });
        assert!(client
            .shared_boundary_rows(start, end, None)
            .await
            .unwrap()
            .unwrap()
            .is_empty());
        client.shared_boundary_rows(start, end, None).await.unwrap();
        valid.assert_hits(1);
        client
            .shared_boundary_rows(start, end, Some(Instant::now()))
            .await
            .unwrap();
        valid.assert_hits(2);
        assert!(client.ranges.lock().await.entries.len() <= CAPACITY);
    }

    #[tokio::test]
    async fn shared_boundaries_reject_incomplete_pages_and_isolate_connections() {
        let server = MockServer::start();
        let mut partial = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200)
                .delay(Duration::from_millis(30))
                .json_body(json!({"data":[],"total":1,"page":1,"page_size":1000,"total_pages":1}));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let start = "2024-02-29T00:00:00Z".parse().unwrap();
        let end = "2024-02-29T02:00:00Z".parse().unwrap();
        let (a, b) = tokio::join!(
            client.shared_boundary_rows(start, end, None),
            client.shared_boundary_rows(start, end, None)
        );
        assert!(a.is_err() && b.is_err());
        partial.assert_hits(1);
        partial.delete();
        let valid = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200)
                .json_body(json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}));
        });
        client.shared_boundary_rows(start, end, None).await.unwrap();
        let mut same = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        same.reuse_daily_cache(&client);
        same.shared_boundary_rows(start, end, None).await.unwrap();
        valid.assert_hits(1);
        let mut other = Client::new(&server.base_url(), "sk-other".into()).unwrap();
        other.reuse_daily_cache(&client);
        other.shared_boundary_rows(start, end, None).await.unwrap();
        valid.assert_hits(2);
    }
}

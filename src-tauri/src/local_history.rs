//! Cache persistant du calendrier local, distinct de l'historique UTC des alertes.
use crate::litellm::calendar::{CalendarWindow, DayBounds};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use shared::{HistoryDay, HistoryDetails, HistoryMode};

#[derive(Clone, Serialize, Deserialize)]
struct Amount {
    bounds: DayBounds,
    spend: f64,
    fetched: DateTime<Utc>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LocalHistory {
    version: u32,
    source_id: String,
    pub window: CalendarWindow,
    amounts: Vec<Amount>,
    synced_day: Option<NaiveDate>,
    updated_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    error: Option<String>,
}

impl LocalHistory {
    pub fn load(path: Option<&std::path::Path>, source_id: String, window: CalendarWindow) -> Self {
        let cached = path
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|v| serde_json::from_slice::<Self>(&v).ok())
            .filter(|c| {
                c.version == 2
                    && c.source_id == source_id
                    && !source_id.is_empty()
                    && c.amounts
                        .iter()
                        .all(|a| a.spend.is_finite() && a.spend >= 0.0)
            });
        let mut state = cached.unwrap_or_else(|| Self {
            version: 2,
            source_id: source_id.clone(),
            window: window.clone(),
            amounts: vec![],
            synced_day: None,
            updated_at: None,
            error: None,
        });
        state.rebase(source_id, window);
        state
    }
    pub fn rebase(&mut self, source_id: String, window: CalendarWindow) -> bool {
        if self.source_id == source_id && self.window == window {
            return false;
        }
        if self.source_id != source_id {
            self.amounts.clear();
        }
        self.amounts
            .retain(|a| window.days.iter().any(|(_, b)| *b == Some(a.bounds)));
        self.source_id = source_id;
        self.window = window;
        self.synced_day = None;
        self.updated_at = None;
        self.error = None;
        true
    }
    pub fn range(&self, now: DateTime<Utc>, mode: HistoryMode) -> Option<NaiveDate> {
        if mode == HistoryMode::Cached {
            return None;
        }
        if mode == HistoryMode::Force
            || self.error.is_some()
            || self.synced_day != Some(self.window.today)
            || self.window.days[..29]
                .iter()
                .any(|(_, b)| !self.amounts.iter().any(|a| Some(a.bounds) == *b))
        {
            return Some(self.window.days[0].0);
        }
        if self
            .updated_at
            .is_some_and(|last| now >= last && now - last < Duration::seconds(60))
        {
            None
        } else {
            Some(self.window.today)
        }
    }
    pub fn merge_day(&mut self, bounds: DayBounds, spend: f64, fetched: DateTime<Utc>) -> bool {
        if !spend.is_finite()
            || spend < 0.0
            || !self.window.days.iter().any(|(_, b)| *b == Some(bounds))
        {
            return false;
        }
        if let Some(old) = self
            .amounts
            .iter_mut()
            .find(|a| a.bounds.date == bounds.date)
        {
            if old.fetched > fetched {
                return false;
            }
            *old = Amount {
                bounds,
                spend,
                fetched,
            };
        } else {
            self.amounts.push(Amount {
                bounds,
                spend,
                fetched,
            });
        }
        true
    }
    pub fn finish(&mut self, now: DateTime<Utc>, start: NaiveDate, error: Option<String>) {
        if error.is_none() && start == self.window.days[0].0 {
            self.synced_day = Some(self.window.today);
        }
        self.updated_at = Some(now);
        self.error = error;
    }
    pub fn view(&self) -> HistoryDetails {
        HistoryDetails {
            days: self
                .window
                .days
                .iter()
                .map(|(date, bounds)| HistoryDay {
                    date: date.to_string(),
                    spend: self
                        .amounts
                        .iter()
                        .find(|a| Some(a.bounds) == *bounds)
                        .map(|a| a.spend),
                })
                .collect(),
            first_weekday: self.window.days[0].0.weekday().num_days_from_monday() as u8,
            updated_at: self.updated_at.map(|v| v.to_rfc3339()),
            error: self.error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litellm::calendar::CalendarWindow;
    use chrono::{Duration, FixedOffset, NaiveDate, Utc};
    use shared::HistoryMode;

    #[test]
    fn local_calendar_rejects_utc_cache_and_rolls_only_compatible_bounds() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        let now = day.and_hms_opt(12, 0, 0).unwrap().and_utc();
        let path =
            std::env::temp_dir().join(format!("local-history-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"version":1,"source_id":"key","first":"2024-03-02","amounts":[1,1]}"#,
        )
        .unwrap();
        let window = CalendarWindow::new(day, &Utc);
        let mut state = LocalHistory::load(Some(&path), "key".into(), window.clone());
        assert!(state.view().days.iter().all(|d| d.spend.is_none()));
        assert!(state.merge_day(window.days[29].1.unwrap(), 7.0, now));
        state.finish(now, window.days[0].0, None);
        crate::persistence::write(&path, &state).unwrap();
        let restored = LocalHistory::load(Some(&path), "key".into(), window.clone());
        assert_eq!(restored.view().days[29].spend, Some(7.0));
        assert!(
            LocalHistory::load(Some(&path), "other".into(), window.clone())
                .view()
                .days
                .iter()
                .all(|d| d.spend.is_none())
        );
        state.rebase(
            "key".into(),
            CalendarWindow::new(day + Duration::days(1), &Utc),
        );
        assert_eq!(state.view().days[28].spend, Some(7.0));
        assert_eq!(state.view().days[29].spend, None);
        state.rebase(
            "key".into(),
            CalendarWindow::new(
                day + Duration::days(1),
                &FixedOffset::east_opt(7200).unwrap(),
            ),
        );
        assert!(state.view().days.iter().all(|d| d.spend.is_none()));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn local_calendar_freshness_partial_failure_and_late_values() {
        let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let now = day.and_hms_opt(12, 0, 0).unwrap().and_utc();
        let window = CalendarWindow::new(day, &Utc);
        let mut state = LocalHistory::load(None, "key".into(), window.clone());
        assert_eq!(state.range(now, HistoryMode::Cached), None);
        assert_eq!(
            state.range(now, HistoryMode::Refresh),
            Some(window.days[0].0)
        );
        for (_, bounds) in &window.days {
            state.merge_day(bounds.unwrap(), 1.0, now);
        }
        state.finish(now, window.days[0].0, None);
        assert_eq!(
            state.range(now + Duration::seconds(59), HistoryMode::Refresh),
            None
        );
        assert_eq!(
            state.range(now + Duration::seconds(60), HistoryMode::Refresh),
            Some(day)
        );
        assert_eq!(state.range(now, HistoryMode::Force), Some(window.days[0].0));
        assert!(state.merge_day(window.days[29].1.unwrap(), 2.0, now + Duration::seconds(1)));
        assert!(!state.merge_day(window.days[29].1.unwrap(), 99.0, now));
        state.finish(now, day, Some("offline".into()));
        assert_eq!(state.view().days[29].spend, Some(2.0));
        assert!(state.view().error.is_some());
        assert!(state.range(now, HistoryMode::Refresh).is_some());
    }
}

//! Historique borné : transitions déterministes et sérialisation, sans réseau ni horloge implicite.
use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use shared::{HistoryDay, HistoryDetails, HistoryMode};

pub const DAYS: usize = 30;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct History {
    version: u32,
    source_id: String,
    first: NaiveDate,
    amounts: [Option<f64>; DAYS],
    synced_day: Option<NaiveDate>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl History {
    pub fn empty(source_id: String, today: NaiveDate) -> Self {
        Self {
            version: 1,
            source_id,
            first: today - Duration::days(29),
            amounts: [None; DAYS],
            synced_day: None,
            updated_at: None,
        }
    }

    pub fn load(path: Option<&std::path::Path>, source: String, today: NaiveDate) -> Self {
        let cached = path
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|h| {
                h.version == 1
                    && !source.is_empty()
                    && h.source_id == source
                    && h.first <= today - Duration::days(29)
                    && h.amounts
                        .iter()
                        .flatten()
                        .all(|v| v.is_finite() && *v >= 0.0)
            });
        let mut result = cached.unwrap_or_else(|| Self::empty(source, today));
        result.roll(today);
        result
    }

    pub fn roll(&mut self, today: NaiveDate) {
        let first = today - Duration::days(29);
        if first == self.first {
            return;
        }
        let offset = (first - self.first).num_days();
        self.amounts = std::array::from_fn(|i| {
            let old = i as i64 + offset;
            usize::try_from(old)
                .ok()
                .and_then(|index| self.amounts.get(index))
                .copied()
                .flatten()
        });
        self.first = first;
    }

    /// Une synchronisation complète quotidienne ; ensuite aujourd'hui, au plus une fois par minute.
    pub fn range(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        mode: HistoryMode,
    ) -> Option<NaiveDate> {
        let today = now.date_naive();
        if mode == HistoryMode::Cached {
            return None;
        }
        if mode == HistoryMode::Force
            || self.synced_day != Some(today)
            || self.amounts[..29].iter().any(Option::is_none)
        {
            return Some(today - Duration::days(29));
        }
        if self
            .updated_at
            .is_some_and(|last| now >= last && now - last < Duration::seconds(60))
        {
            None
        } else {
            Some(today)
        }
    }

    pub fn merge(
        &mut self,
        now: chrono::DateTime<chrono::Utc>,
        start: NaiveDate,
        amounts: [Option<f64>; DAYS],
    ) {
        self.roll(now.date_naive());
        for (current, incoming) in self.amounts.iter_mut().zip(amounts) {
            if incoming.is_some() {
                *current = incoming;
            }
        }
        if start == self.first {
            self.synced_day = Some(now.date_naive());
        }
        self.updated_at = Some(now);
    }

    pub fn view(&self, error: Option<String>) -> HistoryDetails {
        HistoryDetails {
            days: self
                .amounts
                .iter()
                .enumerate()
                .map(|(i, spend)| HistoryDay {
                    date: (self.first + Duration::days(i as i64)).to_string(),
                    spend: *spend,
                })
                .collect(),
            first_weekday: self.first.weekday().num_days_from_monday() as u8,
            updated_at: self.updated_at.map(|time| time.to_rfc3339()),
            error,
        }
    }
}

/// Agrège uniquement la plage demandée ; les dates hors plage ne deviennent jamais des cases connues.
pub fn aggregate<'a>(
    today: NaiveDate,
    start: NaiveDate,
    rows: impl IntoIterator<Item = (Option<&'a str>, f64)>,
) -> Result<[Option<f64>; DAYS], String> {
    let first = today - Duration::days(29);
    let mut amounts =
        std::array::from_fn(|i| (first + Duration::days(i as i64) >= start).then_some(0.0));
    for (date, spend) in rows {
        let date =
            crate::increase::parse_day(date).ok_or("Historique des dépenses : date invalide")?;
        if !spend.is_finite() || spend < 0.0 {
            return Err("Historique des dépenses : montant invalide".into());
        }
        if date < start || date > today {
            continue;
        }
        let offset = (date - first).num_days();
        if let Some(Some(value)) = usize::try_from(offset)
            .ok()
            .and_then(|index| amounts.get_mut(index))
        {
            *value += spend;
            if !value.is_finite() {
                return Err("Historique des dépenses : total invalide".into());
            }
        }
    }
    Ok(amounts)
}

/// La détection ne doit jamais assimiler un cache incomplet ou une erreur à un retour à la normale.
pub fn comparison(
    today: NaiveDate,
    details: &HistoryDetails,
) -> Result<crate::increase::Comparison, String> {
    if let Some(error) = &details.error {
        return Err(error.clone());
    }
    let first = today - Duration::days(21);
    let (first, today_text) = (first.to_string(), today.to_string());
    let known = details
        .days
        .iter()
        .filter(|d| d.date >= first && d.date < today_text)
        .filter(|d| d.spend.is_some())
        .count();
    if known != 21 {
        return Err("Historique insuffisant pour comparer les dépenses".into());
    }
    crate::increase::compare(
        today,
        details
            .days
            .iter()
            .filter_map(|d| d.spend.map(|v| (Some(d.date.as_str()), v))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }
    fn noon(value: &str) -> chrono::DateTime<chrono::Utc> {
        day(value).and_hms_opt(12, 0, 0).unwrap().and_utc()
    }

    #[test]
    fn thirty_days_cross_month_year_and_leap_day_without_growing() {
        for end in ["2026-01-10", "2024-03-10", "2026-03-10"] {
            let now = noon(end);
            let mut history = History::empty("source".into(), now.date_naive());
            history.merge(now, history.first, [Some(2.0); DAYS]);
            let previous_first = history.first;
            history.roll(now.date_naive() + Duration::days(1));
            let view = history.view(None);
            assert_eq!(view.days.len(), 30);
            assert_eq!(
                view.days[0].date,
                (previous_first + Duration::days(1)).to_string()
            );
            assert_eq!(view.days[28].spend, Some(2.0));
            assert_eq!(view.days[29].spend, None);
            history.roll(now.date_naive() + Duration::days(40));
            assert!(history.amounts.iter().all(Option::is_none));
        }
        let history = History::empty("source".into(), day("2024-03-10"));
        assert!(history
            .view(None)
            .days
            .iter()
            .any(|d| d.date == "2024-02-29"));
    }

    #[test]
    fn aggregation_fills_only_requested_dates_and_sums_duplicates_in_utc() {
        let today = day("2026-01-10");
        let full = aggregate(
            today,
            today - Duration::days(29),
            [
                (Some("2025-12-12"), 2.0),
                (Some("2025-12-12"), 3.0),
                (Some("2026-01-11T01:00:00+02:00"), 7.0),
                (Some("2025-12-11"), 900.0),
                (Some("2026-01-11"), 900.0),
            ],
        )
        .unwrap();
        assert_eq!(full[0], Some(5.0));
        assert_eq!(full[1], Some(0.0));
        assert_eq!(full[29], Some(7.0));
        let today_only = aggregate(today, today, [(Some("2026-01-10"), 9.0)]).unwrap();
        assert!(today_only[..29].iter().all(Option::is_none));
        assert_eq!(today_only[29], Some(9.0));
        for row in [
            (None, 1.0),
            (Some("wrong"), 1.0),
            (Some("2026-01-10"), -1.0),
            (Some("2026-01-10"), f64::INFINITY),
        ] {
            assert!(aggregate(today, today, [row]).is_err());
        }
        assert!(aggregate(today, today, [(Some("2026-01-10"), f64::MAX); 2]).is_err());
    }

    #[test]
    fn freshness_manual_refresh_and_corrections_preserve_known_completed_days() {
        let now = noon("2026-10-13");
        let today = now.date_naive();
        let first = today - Duration::days(29);
        let mut history = History::empty("source".into(), today);
        assert_eq!(history.range(now, HistoryMode::Cached), None);
        assert_eq!(history.range(now, HistoryMode::Refresh), Some(first));
        history.merge(now, first, [Some(1.0); DAYS]);
        assert_eq!(
            history.range(now + Duration::seconds(59), HistoryMode::Refresh),
            None
        );
        assert_eq!(
            history.range(now + Duration::seconds(60), HistoryMode::Refresh),
            Some(today)
        );
        assert_eq!(history.range(now, HistoryMode::Force), Some(first));
        let update = aggregate(today, today, [(Some("2026-10-13"), 2.0)]).unwrap();
        history.merge(now + Duration::seconds(60), today, update);
        assert_eq!(history.amounts[0], Some(1.0));
        assert_eq!(history.amounts[29], Some(2.0));
        history.merge(now + Duration::seconds(90), first, [Some(0.5); DAYS]);
        assert_eq!(history.amounts[0], Some(0.5));
        assert_eq!(
            history.range(now + Duration::days(1), HistoryMode::Refresh),
            Some(first + Duration::days(1))
        );
    }

    #[test]
    fn comparison_uses_twenty_one_completed_days_and_refuses_unknown_or_failed_data() {
        let now = noon("2026-10-13");
        let mut history = History::empty("source".into(), now.date_naive());
        assert!(comparison(now.date_naive(), &history.view(None)).is_err());
        let mut amounts = [Some(1.0); DAYS];
        amounts[29] = Some(1000.0);
        history.merge(now, history.first, amounts);
        assert_eq!(
            comparison(now.date_naive(), &history.view(None)).unwrap(),
            crate::increase::Comparison {
                recent: 7.0,
                reference: 7.0
            }
        );
        assert!(comparison(now.date_naive(), &history.view(Some("offline".into()))).is_err());
    }

    #[test]
    fn cache_roundtrip_rejects_wrong_sources_versions_invalid_values_and_corruption() {
        let path = std::env::temp_dir().join(format!("history-{}.json", uuid::Uuid::new_v4()));
        let now = noon("2026-10-13");
        let mut original = History::empty("source".into(), now.date_naive());
        original.merge(now, original.first, [Some(1.0); DAYS]);
        crate::persistence::write(&path, &original).unwrap();
        assert_eq!(
            History::load(Some(&path), "source".into(), now.date_naive()),
            original
        );
        assert!(History::load(Some(&path), "other".into(), now.date_naive())
            .amounts
            .iter()
            .all(Option::is_none));
        let mut wrong = original.clone();
        wrong.version = 2;
        crate::persistence::write(&path, &wrong).unwrap();
        assert!(
            History::load(Some(&path), "source".into(), now.date_naive())
                .updated_at
                .is_none()
        );
        wrong = original;
        wrong.amounts[0] = Some(-1.0);
        crate::persistence::write(&path, &wrong).unwrap();
        assert!(
            History::load(Some(&path), "source".into(), now.date_naive())
                .updated_at
                .is_none()
        );
        std::fs::write(&path, b"invalid").unwrap();
        assert!(
            History::load(Some(&path), "source".into(), now.date_naive())
                .updated_at
                .is_none()
        );
        std::fs::remove_file(path).unwrap();
    }
}

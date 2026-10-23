//! Agrégats locaux persistants, jamais de lignes de requêtes ni de secrets.
use crate::litellm::calendar::DayBounds;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use shared::{CachedDaily, DailyCosts, DailyUsage, ModelCost};

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    bounds: DayBounds,
    costs: DailyCosts,
    usage: Option<DailyUsage>,
    fetched: DateTime<Utc>,
    usage_fetched: Option<DateTime<Utc>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DailyStore {
    version: u32,
    source_id: String,
    entries: Vec<Entry>,
}

impl DailyStore {
    pub fn load(path: Option<&std::path::Path>, source_id: String) -> Self {
        path.and_then(|p| std::fs::read(p).ok())
            .and_then(|v| serde_json::from_slice::<Self>(&v).ok())
            .filter(|s| {
                s.version == 2
                    && s.source_id == source_id
                    && !source_id.is_empty()
                    && s.entries.len() <= 30
                    && s.entries
                        .iter()
                        .all(|e| valid(e.bounds, &e.costs, e.usage.as_ref()))
            })
            .unwrap_or(Self {
                version: 2,
                source_id,
                entries: vec![],
            })
    }
    pub fn get(&self, bounds: DayBounds, now: DateTime<Utc>) -> Option<CachedDaily> {
        let today = now.with_timezone(&chrono::Local).date_naive();
        if bounds.date > today || bounds.date < today - Duration::days(29) {
            return None;
        }
        let entry = self
            .entries
            .iter()
            .find(|e| e.bounds == bounds && e.fetched <= now)?;
        let ttl = Duration::seconds(if bounds.date == today { 60 } else { 900 });
        Some(CachedDaily {
            costs: entry.costs.clone(),
            usage: entry.usage.clone(),
            usage_fetched_at: entry.usage_fetched.map(|t| t.to_rfc3339()),
            stale: now - entry.fetched >= ttl,
            usage_stale: entry
                .usage_fetched
                .is_none_or(|t| t > now || now - t >= ttl)
                || entry.usage.as_ref().is_some_and(|u| {
                    u.models.len() != entry.costs.models.len()
                        || u.models.iter().any(|m| {
                            !entry
                                .costs
                                .models
                                .iter()
                                .any(|c| c.model == m.model && (c.spend - m.spend).abs() <= 1e-8)
                        })
                }),
            fetched_at: entry.fetched.to_rfc3339(),
        })
    }
    pub fn put(
        &mut self,
        bounds: DayBounds,
        costs: Option<DailyCosts>,
        usage: Option<DailyUsage>,
        fetched: DateTime<Utc>,
    ) -> bool {
        let costs = match (&usage, costs) {
            (Some(usage), _) => DailyCosts {
                date: usage.date.clone(),
                models: usage
                    .models
                    .iter()
                    .map(|m| ModelCost {
                        model: m.model.clone(),
                        spend: m.spend,
                    })
                    .collect(),
            },
            (None, Some(costs)) => costs,
            _ => return false,
        };
        if !valid(bounds, &costs, usage.as_ref()) {
            return false;
        }
        if let Some(old) = self.entries.iter_mut().find(|e| e.bounds == bounds) {
            let mut changed = false;
            if usage.is_some() && old.usage_fetched.is_none_or(|t| fetched > t) {
                old.usage = usage;
                old.usage_fetched = Some(fetched);
                changed = true;
            }
            // Un aperçu identique n'efface pas les tokens validés et ne prolonge pas leur TTL.
            if fetched > old.fetched {
                old.costs = costs;
                old.fetched = fetched;
                changed = true;
            }
            return changed;
        }
        self.entries.retain(|e| e.bounds.date != bounds.date);
        self.entries.push(Entry {
            bounds,
            costs,
            usage_fetched: usage.as_ref().map(|_| fetched),
            usage,
            fetched,
        });
        self.entries.sort_by_key(|e| e.bounds.date);
        if self.entries.len() > 30 {
            self.entries.remove(0);
        }
        true
    }
}

fn valid(bounds: DayBounds, costs: &DailyCosts, usage: Option<&DailyUsage>) -> bool {
    bounds.start < bounds.end
        && costs.date == bounds.date.to_string()
        && costs
            .models
            .iter()
            .all(|m| m.spend.is_finite() && m.spend >= 0.0)
        && usage.is_none_or(|u| {
            u.date == costs.date
                && u.models
                    .iter()
                    .all(|m| m.spend.is_finite() && m.spend >= 0.0)
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn persisted_usage_stays_fresh_after_rounding_only_cost_preview() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let now = day
            .succ_opt()
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let bounds = day_bounds(day, &Utc).unwrap();
        let mut store = DailyStore::load(None, "source".into());
        store.put(
            bounds,
            None,
            Some(DailyUsage {
                date: day.to_string(),
                models: vec![ModelUsage {
                    model: "m".into(),
                    spend: 23.301333660000004,
                    total_tokens: 100,
                    ..Default::default()
                }],
            }),
            now,
        );
        store.put(
            bounds,
            Some(DailyCosts {
                date: day.to_string(),
                models: vec![ModelCost {
                    model: "m".into(),
                    spend: 23.30133365999999,
                }],
            }),
            None,
            now + Duration::milliseconds(3),
        );
        let cached = store.get(bounds, now + Duration::seconds(1)).unwrap();
        assert!(!cached.usage_stale);
        assert_eq!(cached.usage.unwrap().models[0].total_tokens, 100);
        assert!(
            !store
                .get(bounds, now + Duration::seconds(899))
                .unwrap()
                .usage_stale
        );
        assert!(
            store
                .get(bounds, now + Duration::seconds(900))
                .unwrap()
                .usage_stale
        );
        store.put(
            bounds,
            Some(DailyCosts {
                date: day.to_string(),
                models: vec![ModelCost {
                    model: "m".into(),
                    spend: 24.0,
                }],
            }),
            None,
            now + Duration::seconds(2),
        );
        assert!(
            store
                .get(bounds, now + Duration::seconds(3))
                .unwrap()
                .usage_stale
        );
    }

    use super::*;
    use crate::litellm::calendar::day_bounds;
    use chrono::{Duration, FixedOffset, NaiveDate, Utc};
    use shared::{DailyCosts, DailyUsage, ModelCost, ModelUsage};

    #[test]
    fn persisted_costs_and_usage_survive_both_completion_orders() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        let now = day.and_hms_opt(12, 0, 0).unwrap().and_utc();
        let bounds = day_bounds(day, &Utc).unwrap();
        let costs = DailyCosts {
            date: day.to_string(),
            models: vec![ModelCost {
                model: "m".into(),
                spend: 2.0,
            }],
        };
        let usage = DailyUsage {
            date: day.to_string(),
            models: vec![ModelUsage {
                model: "m".into(),
                spend: 2.0,
                total_tokens: 10,
                ..Default::default()
            }],
        };
        for usage_first in [false, true] {
            let mut store = DailyStore::load(None, "source".into());
            if usage_first {
                store.put(bounds, None, Some(usage.clone()), now);
            }
            store.put(
                bounds,
                Some(costs.clone()),
                None,
                now + Duration::seconds(2),
            );
            if !usage_first {
                store.put(bounds, None, Some(usage.clone()), now);
            }
            assert_eq!(
                store.get(bounds, now + Duration::seconds(3)).unwrap().usage,
                Some(usage.clone())
            );
        }
    }

    #[test]
    fn persisted_daily_cache_restores_costs_tokens_and_checks_source_bounds_ttl() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        let now = day.and_hms_opt(12, 0, 0).unwrap().and_utc();
        let bounds = day_bounds(day, &FixedOffset::east_opt(7200).unwrap()).unwrap();
        let path = std::env::temp_dir().join(format!("daily-{}.json", uuid::Uuid::new_v4()));
        let costs = DailyCosts {
            date: day.to_string(),
            models: vec![ModelCost {
                model: "m".into(),
                spend: 2.0,
            }],
        };
        let usage = DailyUsage {
            date: day.to_string(),
            models: vec![ModelUsage {
                model: "m".into(),
                spend: 2.0,
                total_tokens: 10,
                ..Default::default()
            }],
        };
        let mut store = DailyStore::load(None, "source".into());
        assert!(store.get(bounds, now).is_none());
        store.put(bounds, Some(costs.clone()), None, now);
        crate::persistence::write(&path, &store).unwrap();
        let mut restored = DailyStore::load(Some(&path), "source".into());
        assert_eq!(restored.get(bounds, now).unwrap().costs, costs);
        assert!(restored.get(bounds, now).unwrap().usage.is_none());
        restored.put(bounds, None, Some(usage.clone()), now);
        crate::persistence::write(&path, &restored).unwrap();
        let restored = DailyStore::load(Some(&path), "source".into());
        assert_eq!(restored.get(bounds, now).unwrap().usage, Some(usage));
        assert!(
            !restored
                .get(bounds, now + Duration::seconds(59))
                .unwrap()
                .stale
        );
        assert!(
            restored
                .get(bounds, now + Duration::seconds(61))
                .unwrap()
                .stale
        );
        assert!(restored.get(day_bounds(day, &Utc).unwrap(), now).is_none());
        assert!(DailyStore::load(Some(&path), "other".into())
            .get(bounds, now)
            .is_none());
        assert!(restored.get(bounds, now + Duration::days(30)).is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persisted_daily_cache_uses_dst_bounds_and_past_day_ttl() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        let bounds = DayBounds {
            date: day,
            start: "2024-03-30T23:00:00Z".parse().unwrap(),
            end: "2024-03-31T22:00:00Z".parse().unwrap(),
        };
        assert_eq!((bounds.end - bounds.start).num_hours(), 23);
        let now = (day + Duration::days(1))
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let mut store = DailyStore::load(None, "source".into());
        store.put(
            bounds,
            Some(DailyCosts {
                date: day.to_string(),
                models: vec![],
            }),
            None,
            now,
        );
        assert!(
            !store
                .get(bounds, now + Duration::seconds(899))
                .unwrap()
                .stale
        );
        assert!(
            store
                .get(bounds, now + Duration::seconds(900))
                .unwrap()
                .stale
        );
        assert!(store
            .get(
                day_bounds(day, &FixedOffset::east_opt(7200).unwrap()).unwrap(),
                now
            )
            .is_none());
    }

    #[test]
    fn persisted_daily_cache_rejects_old_invalid_values_and_bounds_capacity() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        let now = day.and_hms_opt(12, 0, 0).unwrap().and_utc();
        let bounds = day_bounds(day, &Utc).unwrap();
        let mut store = DailyStore::load(None, "source".into());
        let cost = |date: NaiveDate, spend| DailyCosts {
            date: date.to_string(),
            models: vec![ModelCost {
                model: "m".into(),
                spend,
            }],
        };
        store.put(bounds, Some(cost(day, 2.0)), None, now);
        store.put(
            bounds,
            Some(cost(day, -1.0)),
            None,
            now + Duration::seconds(1),
        );
        store.put(
            bounds,
            Some(cost(day, 99.0)),
            None,
            now - Duration::seconds(1),
        );
        assert_eq!(store.get(bounds, now).unwrap().costs.models[0].spend, 2.0);
        for offset in 1..=35 {
            let date = day - Duration::days(offset);
            store.put(
                day_bounds(date, &Utc).unwrap(),
                Some(cost(date, 1.0)),
                None,
                now,
            );
        }
        assert_eq!(store.entries.len(), 30);
    }
}

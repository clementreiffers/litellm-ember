//! Frontières exactes des journées locales et identité de la fenêtre du calendrier.
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct DayBounds {
    pub date: NaiveDate,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

pub(crate) fn day_bounds(date: NaiveDate, zone: &impl TimeZone) -> Option<DayBounds> {
    Some(DayBounds {
        date,
        start: zone
            .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
            .earliest()?
            .with_timezone(&Utc),
        end: zone
            .from_local_datetime(&date.succ_opt()?.and_hms_opt(0, 0, 0)?)
            .earliest()?
            .with_timezone(&Utc),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CalendarWindow {
    pub today: NaiveDate,
    pub days: Vec<(NaiveDate, Option<DayBounds>)>,
    pub id: String,
}

impl CalendarWindow {
    pub fn new(today: NaiveDate, zone: &impl TimeZone) -> Self {
        let days: Vec<_> = (0..30)
            .map(|i| {
                let date = today - Duration::days(29 - i);
                (date, day_bounds(date, zone))
            })
            .collect();
        let id = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&days).expect("dates sérialisables"))
        );
        Self { today, days, id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litellm::tests::{d, ShiftZone};
    use chrono::{Duration, FixedOffset, Utc};

    #[test]
    fn local_bounds_cover_dst_and_missing_midnight() {
        for (date, before, after, hour, hours) in [
            ("2024-03-31", 3600, 7200, 1, Some(23)),
            ("2024-10-27", 7200, 3600, 1, Some(25)),
            ("2024-03-31", 0, 3600, 0, None),
            ("2024-10-27", 3600, 0, 0, Some(25)),
        ] {
            let day = d(date);
            let zone = ShiftZone {
                at: day.and_hms_opt(hour, 0, 0).unwrap(),
                before: FixedOffset::east_opt(before).unwrap(),
                after: FixedOffset::east_opt(after).unwrap(),
            };
            let bounds = day_bounds(day, &zone);
            assert_eq!(
                bounds.as_ref().map(|b| (b.end - b.start).num_hours()),
                hours
            );
            if hour == 0 && hours.is_some() {
                assert_eq!(
                    bounds.unwrap().start,
                    day.and_hms_opt(0, 0, 0).unwrap().and_utc() - Duration::hours(1)
                );
            }
        }
        let day = d("2024-03-31");
        let utc = CalendarWindow::new(day, &Utc);
        assert_eq!(utc.days.len(), 30);
        assert_eq!(
            utc.id,
            CalendarWindow::new(day, &FixedOffset::east_opt(0).unwrap()).id
        );
        assert_ne!(
            utc.id,
            CalendarWindow::new(day, &FixedOffset::east_opt(7200).unwrap()).id
        );
    }
}

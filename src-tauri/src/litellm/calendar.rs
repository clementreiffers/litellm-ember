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
        start: day_start(date, zone)?,
        end: day_start(date.succ_opt()?, zone)?,
    })
}

/// Premier instant local valide de la journée : minuit, ou la fin d'un saut horaire qui l'omet.
fn day_start(date: NaiveDate, zone: &impl TimeZone) -> Option<DateTime<Utc>> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    if let Some(start) = zone.from_local_datetime(&midnight).earliest() {
        return Some(start.with_timezone(&Utc));
    }
    // Minuit tombe dans un saut : chercher le premier instant UTC dont la date locale est
    // déjà ce jour. Les offsets sont des secondes entières et restent sous ±26 h, donc une
    // recherche sur des secondes entières depuis minuit UTC trouve l'instant exact.
    let is_today = |t: DateTime<Utc>| zone.from_utc_datetime(&t.naive_utc()).date_naive() >= date;
    let origin = midnight.and_utc();
    let at = |seconds: i64| origin.checked_add_signed(Duration::seconds(seconds));
    let (mut before, mut after) = (-26 * 3600_i64, 26 * 3600_i64);
    if at(before).is_none_or(is_today) || !at(after).is_some_and(is_today) {
        return None;
    }
    while after - before > 1 {
        let middle = before + (after - before) / 2;
        if at(middle).is_some_and(is_today) {
            after = middle;
        } else {
            before = middle;
        }
    }
    at(after)
}

/// Une journée n'est figée qu'une fois validée après sa fin, marge comprise ; le temps
/// écoulé se mesure à l'horloge murale pour compter la mise en veille du Mac.
pub(crate) fn is_fresh(
    bounds: &DayBounds,
    validated_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    let ttl = if validated_at >= bounds.end + Duration::seconds(60) {
        Duration::seconds(900)
    } else {
        Duration::seconds(60)
    };
    validated_at <= now && now - validated_at < ttl
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
    fn missing_midnight_starts_at_the_exact_end_of_the_jump() {
        // Saut à 23:59:13 UTC (offset −47 s) vers +2235 s : minuit locale n'existe pas.
        let day = d("2024-03-31");
        let jump = day.and_hms_opt(0, 0, 0).unwrap() - Duration::seconds(47);
        let zone = ShiftZone {
            at: jump,
            before: FixedOffset::east_opt(-47).unwrap(),
            after: FixedOffset::east_opt(2235).unwrap(),
        };
        let bounds = day_bounds(day, &zone).unwrap();
        assert_eq!(bounds.start, jump.and_utc());
        assert_eq!(
            zone.from_utc_datetime(&bounds.start.naive_utc())
                .naive_local(),
            day.and_hms_opt(0, 36, 28).unwrap()
        );
        // Aux limites du calendrier, aucune arithmétique ne déborde.
        let edge = ShiftZone {
            at: NaiveDate::MAX.and_hms_opt(0, 0, 0).unwrap(),
            ..zone
        };
        assert!(day_bounds(NaiveDate::MAX, &edge).is_none());
    }

    #[test]
    fn day_validated_before_its_end_expires_like_today() {
        let bounds = day_bounds(d("2024-02-29"), &Utc).unwrap();
        let before = bounds.end - Duration::seconds(30);
        assert!(is_fresh(&bounds, before, before + Duration::seconds(59)));
        assert!(!is_fresh(&bounds, before, before + Duration::seconds(90)));
        let late = bounds.end + Duration::seconds(30);
        assert!(!is_fresh(&bounds, late, late + Duration::seconds(60)));
        let final_ = bounds.end + Duration::seconds(60);
        assert!(is_fresh(&bounds, final_, final_ + Duration::seconds(899)));
        assert!(!is_fresh(&bounds, final_, final_ + Duration::seconds(900)));
        assert!(!is_fresh(&bounds, final_, final_ - Duration::seconds(1)));
    }

    #[test]
    fn local_bounds_cover_dst_and_missing_midnight() {
        for (date, before, after, hour, hours) in [
            ("2024-03-31", 3600, 7200, 1, Some(23)),
            ("2024-10-27", 7200, 3600, 1, Some(25)),
            ("2024-03-31", 0, 3600, 0, Some(23)),
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
            if hour == 0 {
                // Minuit répété : la première occurrence ; minuit absent : la fin du saut.
                let shift = if before > after { -1 } else { 0 };
                assert_eq!(
                    bounds.unwrap().start,
                    day.and_hms_opt(0, 0, 0).unwrap().and_utc() + Duration::hours(shift)
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

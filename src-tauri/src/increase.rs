//! Détection hebdomadaire déterministe : ni horloge, ni réseau, ni notification.
use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use shared::SpendIncreaseSettings;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Comparison {
    pub recent: f64,
    pub reference: f64,
}

impl Comparison {
    pub fn exceeds(self, settings: SpendIncreaseSettings) -> bool {
        self.recent - self.reference >= settings.min_usd
            && self.recent >= self.reference * (1.0 + f64::from(settings.percent) / 100.0)
    }

    pub fn message(self) -> String {
        let change = if self.reference > 0.0 {
            format!(
                "hausse de {:.0} %",
                (self.recent / self.reference - 1.0) * 100.0
            )
        } else {
            "aucune dépense pendant la période de référence".into()
        };
        format!("${:.2} sur les 7 derniers jours terminés, contre ${:.2} par semaine habituellement ({change}).", self.recent, self.reference)
    }
}

pub(crate) fn parse_day(value: Option<&str>) -> Option<NaiveDate> {
    value.and_then(|value| {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .or_else(|| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|date| date.with_timezone(&chrono::Utc).date_naive())
            })
    })
}

/// Une case par journée UTC ; les doublons sont additionnés et les jours absents valent zéro.
pub fn compare<'a>(
    today: NaiveDate,
    rows: impl IntoIterator<Item = (Option<&'a str>, f64)>,
) -> Result<Comparison, String> {
    let first = today - Duration::days(21);
    let mut days = [0.0; 21];
    for (date, spend) in rows {
        let date = parse_day(date).ok_or("Historique des dépenses : date invalide")?;
        if !spend.is_finite() || spend < 0.0 {
            return Err("Historique des dépenses : montant invalide".into());
        }
        let offset = (date - first).num_days();
        if (0..21).contains(&offset) {
            days[offset as usize] += spend;
        }
    }
    let reference = days[..14].iter().sum::<f64>() / 2.0;
    let recent = days[14..].iter().sum::<f64>();
    if !reference.is_finite() || !recent.is_finite() {
        return Err("Historique des dépenses : total invalide".into());
    }
    Ok(Comparison { recent, reference })
}

/// État léger persisté avec les autres notifications ; aucun historique de requêtes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub day: Option<NaiveDate>,
    pub settings: Option<SpendIncreaseSettings>,
    pub fired: bool,
}

impl State {
    pub fn due(&self, today: NaiveDate, settings: SpendIncreaseSettings) -> bool {
        settings.enabled && (self.day != Some(today) || self.settings != Some(settings))
    }

    /// Transition sans effet ; l'appelant ne la valide qu'après un éventuel envoi accepté.
    pub fn evaluate(
        &self,
        day: NaiveDate,
        settings: SpendIncreaseSettings,
        comparison: Comparison,
    ) -> (Self, bool) {
        let exceeds = comparison.exceeds(settings);
        (
            Self {
                day: Some(day),
                settings: Some(settings),
                fired: exceeds,
            },
            exceeds && !self.fired,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn timestamp_offsets_are_converted_to_utc_before_bucketing() {
        let result = compare(
            day("2026-01-08"),
            [
                (Some("2026-01-08T01:00:00+02:00"), 3.0),
                (Some("2026-01-01T01:00:00+02:00"), 4.0),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Comparison {
                recent: 3.0,
                reference: 2.0
            }
        );
        assert!(compare(day("2026-01-08"), [(Some("2026-01-01-invalid"), 1.0)]).is_err());
    }

    #[test]
    fn windows_are_disjoint_and_exclude_today_across_year_boundary() {
        let rows = [
            (Some("2025-12-31"), 5.0),
            (Some("2025-12-18"), 15.0),
            (Some("2026-01-01"), 7.0),
            (Some("2026-01-07"), 8.0),
            (Some("2026-01-07"), 1.0),
            (Some("2026-01-08"), 100.0),
            (Some("2025-12-17"), 100.0),
        ];
        assert_eq!(
            compare(day("2026-01-08"), rows).unwrap(),
            Comparison {
                reference: 10.0,
                recent: 16.0
            }
        );
        assert_eq!(
            compare(day("2026-01-09"), rows).unwrap(),
            Comparison {
                reference: 6.0,
                recent: 109.0
            }
        );
    }

    #[test]
    fn rejects_bad_dates_amounts_and_overflow() {
        for row in [
            (None, 1.0),
            (Some("bad"), 1.0),
            (Some("2026-01-01"), -1.0),
            (Some("2026-01-01"), f64::NAN),
            (Some("2026-01-01"), f64::INFINITY),
        ] {
            assert!(compare(day("2026-01-08"), [row]).is_err());
        }
        assert!(compare(day("2026-01-08"), [(Some("2026-01-01"), f64::MAX); 2]).is_err());
    }

    #[test]
    fn both_inclusive_thresholds_are_required_and_zero_reference_is_supported() {
        let settings = SpendIncreaseSettings::default();
        for (reference, recent, expected) in [
            (10.0, 15.0, true),
            (10.0, 14.99, false),
            (2.0, 4.99, false),
            (2.0, 5.0, true),
            (0.0, 3.0, true),
            (0.0, 2.99, false),
            (0.0, 0.0, false),
        ] {
            assert_eq!(Comparison { reference, recent }.exceeds(settings), expected);
        }
        assert_eq!(
            compare(day("2026-01-08"), []).unwrap(),
            Comparison {
                reference: 0.0,
                recent: 0.0
            }
        );
    }

    #[test]
    fn alerts_once_until_recovery_and_retains_episode_when_thresholds_change() {
        let date = day("2026-01-08");
        let cfg = SpendIncreaseSettings::default();
        let high = Comparison {
            reference: 10.0,
            recent: 20.0,
        };
        let normal = Comparison {
            reference: 10.0,
            recent: 10.0,
        };
        let (state, send) = State::default().evaluate(date, cfg, high);
        assert!(send);
        assert!(!state.due(date, cfg));
        assert!(state.due(date + Duration::days(1), cfg));
        assert!(!state.due(
            date,
            SpendIncreaseSettings {
                enabled: false,
                ..cfg
            }
        ));
        let changed = SpendIncreaseSettings { percent: 60, ..cfg };
        assert!(state.due(date, changed));
        let (state, send) = state.evaluate(date, changed, high);
        assert!(!send);
        let (state, send) = state.evaluate(date, cfg, normal);
        assert!(!send);
        assert!(state.evaluate(date, cfg, high).1);
    }

    #[test]
    fn notification_describes_zero_and_nonzero_reference() {
        assert!(Comparison {
            reference: 10.0,
            recent: 16.0
        }
        .message()
        .contains("60 %"));
        assert!(Comparison {
            reference: 0.0,
            recent: 3.0
        }
        .message()
        .contains("aucune dépense"));
    }
}

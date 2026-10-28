//! Navigation et sélection des données de consommation quotidienne.
use leptos::prelude::*;
use shared::{DailyUsage, ModelUsage, Stats};

pub fn visible_usage(
    selected: Option<&str>,
    today: &str,
    stats: &Stats,
    historical: Option<&DailyUsage>,
) -> Option<Vec<ModelUsage>> {
    match selected {
        Some(date) => historical
            .filter(|usage| usage.date == date)
            .map(|usage| usage.models.clone()),
        None => (stats.today_date.as_deref() == Some(today)).then(|| stats.today.clone()),
    }
}

pub fn normalize_selection(date: &str, today: &str) -> Option<String> {
    (date != today).then(|| date.to_owned())
}

pub fn daily_total(
    full: Option<&[ModelUsage]>,
    costs: Option<&[shared::ModelCost]>,
    known: Option<f64>,
) -> Option<f64> {
    full.map(|rows| rows.iter().map(|r| r.spend).sum())
        .or_else(|| costs.map(|rows| rows.iter().map(|r| r.spend).sum()))
        .or(known)
}

/// Des coûts validés après les tokens et qui les contredisent les rendent périmés.
/// Des tokens validés plus tard l'emportent au contraire, quel que soit l'ordre d'arrivée.
pub fn compatible_usage<'a>(
    usage: Option<(&'a DailyUsage, i64)>,
    costs: Option<(&shared::DailyCosts, i64)>,
) -> Option<&'a DailyUsage> {
    let (usage, usage_validated) = usage?;
    let contradicted = costs.is_some_and(|(costs, costs_validated)| {
        costs_validated > usage_validated
            && costs.date == usage.date
            && !shared::costs_match(&usage.models, &costs.models)
    });
    (!contradicted).then_some(usage)
}

pub fn visible_costs(
    selected: Option<&str>,
    full: Option<&[ModelUsage]>,
    preview: Option<&shared::DailyCosts>,
) -> Option<Vec<shared::ModelCost>> {
    if let Some(usage) = full {
        return Some(
            usage
                .iter()
                .map(|m| shared::ModelCost {
                    model: m.model.clone(),
                    spend: m.spend,
                })
                .collect(),
        );
    }
    preview
        .filter(|costs| Some(costs.date.as_str()) == selected)
        .map(|costs| costs.models.clone())
}

fn format_local(date: &js_sys::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date()
    )
}

pub fn local_today() -> String {
    format_local(&js_sys::Date::new_0())
}

/// Identité locale de la date sélectionnée, même en dehors de la fenêtre de l'historique.
pub fn local_day_bounds(value: &str) -> Option<(i64, i64)> {
    calendar_date(value)?;
    let start = js_sys::Date::new(&format!("{value}T00:00:00").into());
    let end = js_sys::Date::new(&start.get_time().into());
    end.set_date(end.get_date() + 1);
    (start.get_time().is_finite() && end.get_time().is_finite())
        .then_some((start.get_time() as i64, end.get_time() as i64))
}

pub fn update_today(today: RwSignal<String>) -> String {
    let current = local_today();
    if today.get_untracked() != current {
        today.set(current.clone());
    }
    current
}

fn calendar_date(value: &str) -> Option<js_sys::Date> {
    if value.len() != 10 {
        return None;
    }
    // Midi évite les frontières de changement d'heure ; aucune conversion ISO UTC.
    let date = js_sys::Date::new(&format!("{value}T12:00:00").into());
    (date.get_time().is_finite() && format_local(&date) == value).then_some(date)
}

#[component]
pub fn DayNavigation(
    selected: RwSignal<Option<String>>,
    today: RwSignal<String>,
    on_select: Callback<Option<String>>,
) -> impl IntoView {
    let displayed = Signal::derive(move || selected.get().unwrap_or_else(|| today.get()));
    let step = move |delta: i32| {
        let current = update_today(today);
        if let Some(date) = calendar_date(&displayed.get_untracked()) {
            date.set_date((date.get_date() as i32 + delta) as u32);
            let next = format_local(&date);
            if next.as_str() >= "0001-01-01" && next <= current {
                on_select.run(normalize_selection(&next, &current));
            }
        }
    };
    view! {
        <div class="day-navigation" role="group" aria-label="Choisir une journée">
            <button type="button" aria-label="Jour précédent" title="Jour précédent"
                disabled=move || displayed.get() == "0001-01-01" on:click=move |_| step(-1)>"‹"</button>
            <input type="date" aria-label="Date de consommation" min="0001-01-01"
                max=move || today.get() prop:value=move || displayed.get()
                on:change:target=move |event| {
                    let value = event.target().value();
                    let current = update_today(today);
                    if value.as_str() >= "0001-01-01" && value <= current && calendar_date(&value).is_some() {
                        on_select.run(normalize_selection(&value, &current));
                    } else {
                        event.target().set_value(&displayed.get_untracked());
                    }
                } />
            <button type="button" aria-label="Jour suivant" title="Jour suivant"
                disabled={move || displayed.get() >= today.get()} on:click=move |_| step(1)>"›"</button>
            <button type="button" class="day-today" disabled=move || selected.with(Option::is_none)
                on:click=move |_| { update_today(today); on_select.run(None); }>"Aujourd'hui"</button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{DailyUsage, ModelUsage, Stats};

    #[test]
    fn daily_total_prioritizes_details_and_preserves_known_zero() {
        assert_eq!(daily_total(None, None, Some(0.0)), Some(0.0));
        let costs = vec![shared::ModelCost {
            model: "m".into(),
            spend: 2.0,
        }];
        let full = vec![ModelUsage {
            spend: 3.0,
            ..Default::default()
        }];
        assert_eq!(daily_total(None, Some(&costs), Some(1.0)), Some(2.0));
        assert_eq!(daily_total(Some(&full), Some(&costs), Some(1.0)), Some(3.0));
        assert_eq!(daily_total(None, None, None), None);
    }

    #[test]
    fn historical_selection_never_reads_live_or_other_day_usage() {
        let stats = Stats {
            today_date: Some("2026-10-22".into()),
            today: vec![ModelUsage {
                spend: 99.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let historical = DailyUsage {
            date: "2024-02-29".into(),
            models: vec![ModelUsage {
                spend: 2.0,
                ..Default::default()
            }],
        };
        assert_eq!(
            visible_usage(Some("2024-02-29"), "2026-10-22", &stats, Some(&historical)).unwrap()[0]
                .spend,
            2.0
        );
        assert_eq!(
            visible_usage(Some("2024-02-28"), "2026-10-22", &stats, Some(&historical)),
            None
        );
        assert_eq!(
            visible_usage(Some("2024-02-29"), "2026-10-22", &stats, None),
            None
        );
        assert_eq!(
            visible_usage(None, "2026-10-22", &stats, Some(&historical)).unwrap()[0].spend,
            99.0
        );
        assert_eq!(
            visible_usage(None, "2026-10-23", &stats, Some(&historical)),
            None
        );
    }

    #[test]
    fn costs_arrive_before_tokens_and_full_usage_takes_precedence() {
        let preview = shared::DailyCosts {
            date: "2024-02-29".into(),
            models: vec![shared::ModelCost {
                model: "m".into(),
                spend: 5.0,
            }],
        };
        assert_eq!(
            visible_costs(Some("2024-02-29"), None, Some(&preview)).unwrap()[0].spend,
            5.0
        );
        assert_eq!(
            visible_costs(Some("2024-02-28"), None, Some(&preview)),
            None
        );
        assert_eq!(visible_costs(None, None, Some(&preview)), None);
        let full = vec![ModelUsage {
            model: "m".into(),
            spend: 6.0,
            total_tokens: 100,
            ..Default::default()
        }];
        assert_eq!(
            visible_costs(Some("2024-02-29"), Some(&full), Some(&preview)).unwrap()[0].spend,
            6.0
        );
        assert_eq!(
            visible_costs(Some("2024-02-29"), Some(&[]), Some(&preview)),
            Some(vec![])
        );
    }

    #[test]
    fn corrected_costs_hide_tokens_whose_spend_no_longer_matches() {
        let date = "2024-02-29";
        let old = DailyUsage {
            date: date.into(),
            models: vec![ModelUsage {
                model: "m".into(),
                spend: 2.0,
                total_tokens: 100,
                ..Default::default()
            }],
        };
        let corrected = shared::DailyCosts {
            date: date.into(),
            models: vec![shared::ModelCost {
                model: "m".into(),
                spend: 9.0,
            }],
        };
        // Coûts corrigés, tokens non rechargés : les anciens tokens ne sont plus fiables.
        assert_eq!(
            compatible_usage(Some((&old, 1)), Some((&corrected, 2))),
            None
        );
        assert_eq!(
            visible_costs(Some(date), None, Some(&corrected)).unwrap()[0].spend,
            9.0
        );
        // Mêmes coûts à l'arrondi près : les tokens validés restent affichés.
        let same = shared::DailyCosts {
            date: date.into(),
            models: vec![shared::ModelCost {
                model: "m".into(),
                spend: 2.0 + 1e-12,
            }],
        };
        assert_eq!(
            compatible_usage(Some((&old, 1)), Some((&same, 2))),
            Some(&old)
        );
        assert_eq!(compatible_usage(Some((&old, 1)), None), Some(&old));
        let mut with_free_bucket = same.clone();
        with_free_bucket.models.push(shared::ModelCost {
            model: "".into(),
            spend: 0.0,
        });
        assert_eq!(
            compatible_usage(Some((&old, 1)), Some((&with_free_bucket, 2))),
            Some(&old)
        );
        let other_day = shared::DailyCosts {
            date: "2024-02-28".into(),
            ..corrected.clone()
        };
        assert_eq!(
            compatible_usage(Some((&old, 1)), Some((&other_day, 2))),
            Some(&old)
        );
        // Tokens complets plus récents qu'un aperçu conservé après un échec : ils l'emportent.
        let fresh = DailyUsage {
            date: date.into(),
            models: vec![ModelUsage {
                model: "m".into(),
                spend: 9.0,
                total_tokens: 900,
                ..Default::default()
            }],
        };
        let kept = shared::DailyCosts {
            date: date.into(),
            models: vec![shared::ModelCost {
                model: "m".into(),
                spend: 2.0,
            }],
        };
        let shown = compatible_usage(Some((&fresh, 3)), Some((&kept, 2)));
        assert_eq!(shown, Some(&fresh));
        assert_eq!(
            visible_costs(Some(date), shown.map(|u| u.models.as_slice()), Some(&kept)).unwrap()[0]
                .spend,
            9.0
        );
    }

    #[test]
    fn choosing_today_restores_live_mode_while_past_dates_stay_fixed() {
        assert_eq!(normalize_selection("2026-10-22", "2026-10-22"), None);
        assert_eq!(
            normalize_selection("2026-10-21", "2026-10-22"),
            Some("2026-10-21".into())
        );
    }
}

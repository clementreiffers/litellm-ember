//! Calendrier GitHub : préparation pure des cases, rendu Leptos sans bibliothèque graphique.
use crate::details::Remote;
use leptos::prelude::*;
use shared::{HistoryDay, HistoryDetails};

/// Indice CSS (zéro neutre, puis quatre quarts du maximum connu).
fn intensity(value: Option<f64>, max: f64) -> &'static str {
    match value {
        None => "unknown",
        Some(v) if v <= 0.0 || max <= 0.0 => "level-0",
        Some(v) if v <= max * 0.25 => "level-1",
        Some(v) if v <= max * 0.5 => "level-2",
        Some(v) if v <= max * 0.75 => "level-3",
        Some(_) => "level-4",
    }
}

fn cells(details: &HistoryDetails) -> Vec<Option<&HistoryDay>> {
    let offset = usize::from(details.first_weekday.min(6));
    let count = details.days.len().min(30);
    let size = (offset + count).div_ceil(7) * 7;
    (0..size)
        .map(|i| {
            i.checked_sub(offset)
                .filter(|i| *i < count)
                .and_then(|i| details.days.get(i))
        })
        .collect()
}

fn label(day: &HistoryDay, today: bool) -> String {
    let amount = day
        .spend
        .map(|v| format!("${v:.2}"))
        .unwrap_or_else(|| "dépense inconnue".into());
    format!(
        "{} : {amount}{}",
        day.date,
        if today { " · en cours" } else { "" }
    )
}

#[component]
pub fn HistoryCalendar(history: RwSignal<Remote<HistoryDetails>>) -> impl IntoView {
    let selected = RwSignal::new(None::<String>);
    view! {
        <section class="card history-card">
            <h2>"Dépenses sur 30 jours"</h2>
            <div class="hint">"Journées UTC · aujourd’hui inclus"</div>
            {move || history.with(|remote| {
                let Some(data) = remote.data.as_ref() else {
                    return view! { <div class="caption">"Chargement de l’historique…"</div> }.into_any();
                };
                let max = data.days.iter().filter_map(|d| d.spend).fold(0.0, f64::max);
                let total: f64 = data.days.iter().filter_map(|d| d.spend).sum();
                let incomplete = data.days.iter().any(|d| d.spend.is_none());
                let today = data.days.last().map(|d| d.date.as_str());
                let layout = cells(data);
                let columns = layout.len() / 7;
                let grid = layout.into_iter().map(|day| match day {
                    Some(day) => {
                        let date = day.date.clone();
                        let focus_date = date.clone();
                        let is_today = Some(day.date.as_str()) == today;
                        let tip = label(day, is_today);
                        let selected_date = date.clone();
                        let pressed_date = date.clone();
                        view! {
                            <button type="button" class=format!("history-cell {}", intensity(day.spend, max))
                                class:today=is_today class:selected=move || selected.with(|value| value.as_ref() == Some(&selected_date))
                                title=tip.clone() aria-label=tip aria-pressed=move || selected.with(|value| value.as_ref() == Some(&pressed_date))
                                on:focus=move |_| selected.set(Some(focus_date.clone()))
                                on:click=move |_| selected.set(Some(date.clone()))>
                                <span class="sr-only">{day.date.clone()}</span>
                            </button>
                        }.into_any()
                    }
                    None => view! { <span class="history-cell empty" aria-hidden="true"></span> }.into_any(),
                }).collect_view();
                view! {
                    <div class="history-total">{format!("${total:.2}")}<span class="caption">{if incomplete { " · total connu, incomplet" } else { " · total sur 30 jours" }}</span></div>
                    <div class="history-layout">
                        <div class="history-weekdays" aria-hidden="true">
                            {["Lun", "Mar", "Mer", "Jeu", "Ven", "Sam", "Dim"].into_iter().map(|d| view! { <span>{d}</span> }).collect_view()}
                        </div>
                        <div class="history-grid" role="group" aria-label="Dépenses quotidiennes en UTC"
                            style=format!("grid-template-columns:repeat({columns},28px)")>{grid}</div>
                    </div>
                    <div class="history-legend" aria-label="Intensité croissante selon les dépenses">
                        <span>"Moins"</span>
                        {(0..=4).map(|n| view! { <span class=format!("history-swatch level-{n}")></span> }).collect_view()}
                        <span>"Plus"</span><span class="history-swatch unknown"></span><span>"Inconnu"</span>
                    </div>
                    <div class="history-detail" aria-live="polite">{move || history.with(|r| {
                        r.data.as_ref().and_then(|data| {
                            let index = selected.with(|selected| selected.as_ref().and_then(|date| data.days.iter().position(|d| &d.date == date)));
                            index.and_then(|i| data.days.get(i)).or_else(|| data.days.last())
                                .map(|day| label(day, data.days.last().is_some_and(|last| last.date == day.date)))
                        }).unwrap_or_default()
                    })}</div>
                    {data.updated_at.as_ref().map(|time| view! { <div class="caption">{format!("Synchronisé : {time}")}</div> })}
                    {data.error.as_ref().map(|error| view! { <div class="error">{format!("Historique conservé · {error}")}</div> })}
                }.into_any()
            })}
            {move || history.with(|r| r.loading).then(|| view! { <div class="caption">"Actualisation…"</div> })}
            {move || history.with(|r| r.error.clone()).map(|error| view! { <div class="error">{error}</div> })}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_keeps_thirty_days_and_only_pads_the_edges() {
        for offset in 0..7 {
            let details = HistoryDetails {
                first_weekday: offset,
                days: (0..30)
                    .map(|i| HistoryDay {
                        date: i.to_string(),
                        spend: Some(i as f64),
                    })
                    .collect(),
                ..Default::default()
            };
            let grid = cells(&details);
            assert_eq!(grid.len() % 7, 0);
            assert_eq!(grid.iter().flatten().count(), 30);
            assert!(grid[..offset as usize].iter().all(Option::is_none));
            assert_eq!(grid[offset as usize].unwrap().date, "0");
            assert_eq!(grid[offset as usize + 29].unwrap().date, "29");
        }
    }
    #[test]
    fn colors_and_labels_distinguish_missing_zero_and_current_day() {
        for (value, expected) in [
            (None, "unknown"),
            (Some(0.0), "level-0"),
            (Some(1.0), "level-1"),
            (Some(2.0), "level-2"),
            (Some(3.0), "level-3"),
            (Some(4.0), "level-4"),
        ] {
            assert_eq!(intensity(value, 4.0), expected);
        }
        assert_eq!(intensity(Some(0.0), 0.0), "level-0");
        let missing = HistoryDay {
            date: "2026-10-13".into(),
            spend: None,
        };
        assert!(label(&missing, true).contains("inconnue · en cours"));
        assert_eq!(
            label(
                &HistoryDay {
                    spend: Some(0.0),
                    ..missing
                },
                false
            ),
            "2026-10-13 : $0.00"
        );
    }
}

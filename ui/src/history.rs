//! Calendrier GitHub : préparation pure des cases, rendu Leptos sans bibliothèque graphique.
use crate::details::Remote;
use leptos::prelude::*;
use shared::{HistoryDay, HistoryDetails};

pub fn accept_snapshot(
    current: &shared::LocalHistorySnapshot,
    next: &shared::LocalHistorySnapshot,
    generation: u64,
) -> bool {
    next.generation == generation
        && (next.generation != current.generation || next.revision > current.revision)
}
pub fn retained_selection(selected: Option<&str>, days: &[HistoryDay]) -> Option<String> {
    selected
        .filter(|date| days.iter().any(|d| d.date == *date))
        .map(str::to_owned)
}

pub fn daily_details_outdated(
    selected: Option<&str>,
    exact: &[HistoryDay],
    usage: Option<&shared::DailyUsage>,
    costs: Option<&shared::DailyCosts>,
) -> bool {
    let Some(date) = selected else {
        return false;
    };
    let Some(spend) = exact.iter().find(|d| d.date == date).and_then(|d| d.spend) else {
        return false;
    };
    let shown = usage
        .filter(|v| v.date == date)
        .map(|v| v.models.iter().map(|m| m.spend).sum::<f64>())
        .or_else(|| {
            costs
                .filter(|v| v.date == date)
                .map(|v| v.models.iter().map(|m| m.spend).sum())
        });
    shown.is_some_and(|v| (v - spend).abs() > 1e-8)
}

#[derive(Clone, Copy)]
pub struct CalendarState {
    pub snapshot: RwSignal<shared::LocalHistorySnapshot>,
    pub selected: RwSignal<Option<String>>,
    pub usage: RwSignal<Remote<shared::DailyUsage>>,
    pub costs: RwSignal<Remote<shared::DailyCosts>>,
    pub epoch: RwSignal<u64>,
}
impl CalendarState {
    pub fn new() -> Self {
        Self {
            snapshot: RwSignal::new(Default::default()),
            selected: RwSignal::new(None),
            usage: RwSignal::new(Default::default()),
            costs: RwSignal::new(Default::default()),
            epoch: RwSignal::new(0),
        }
    }
    pub fn reset_details(self) {
        crate::details::reset(self.usage);
        crate::details::reset(self.costs);
        self.epoch.update(|e| *e += 1);
    }
    pub fn select(self, date: Option<String>) {
        if self.selected.get_untracked() != date {
            self.reset_details();
            self.selected.set(date);
        }
    }
    pub fn accept(self, next: shared::LocalHistorySnapshot, generation: u64) {
        if !self
            .snapshot
            .with_untracked(|old| accept_snapshot(old, &next, generation))
        {
            return;
        }
        let changed = self.snapshot.with_untracked(|old| {
            old.generation != next.generation || old.calendar_id != next.calendar_id
        });
        let outdated = self.usage.with_untracked(|u| {
            self.costs.with_untracked(|c| {
                !u.loading
                    && !c.loading
                    && daily_details_outdated(
                        self.selected.get_untracked().as_deref(),
                        &next.exact_costs,
                        u.data.as_ref(),
                        c.data.as_ref(),
                    )
            })
        });
        if changed || outdated {
            self.reset_details();
        }
        self.select(retained_selection(
            self.selected.get_untracked().as_deref(),
            &next.details.days,
        ));
        self.snapshot.set(next);
    }
    pub fn refresh(self, mode: shared::HistoryMode, generation: Signal<u64>) {
        let requested = generation.get_untracked();
        leptos::task::spawn_local(async move {
            match crate::tauri::get_local_history(mode).await {
                Ok(next)
                    if generation.try_get_untracked() == Some(requested)
                        && !self.snapshot.is_disposed() =>
                {
                    self.accept(next, requested)
                }
                Err(error) if generation.try_get_untracked() == Some(requested) => {
                    self.snapshot.try_update(|s| s.details.error = Some(error));
                }
                _ => {}
            }
        });
    }
    pub fn load_details(self, force: bool, generation: Signal<u64>) {
        if let Some(date) = self.selected.get_untracked() {
            if force {
                self.reset_details();
            }
            let cost_date = date.clone();
            crate::details::load(
                self.costs,
                move || crate::tauri::get_day_costs(cost_date.clone(), force),
                generation,
            );
            crate::details::load(
                self.usage,
                move || crate::tauri::get_day_usage(date.clone(), force),
                generation,
            );
        }
    }
}

#[component]
pub fn DayRecap(state: CalendarState) -> impl IntoView {
    let usage = Signal::derive(move || {
        state.usage.with(|r| {
            r.data
                .as_ref()
                .filter(|v| Some(&v.date) == state.selected.get().as_ref())
                .map(|v| v.models.clone())
        })
    });
    let costs = Signal::derive(move || {
        state.costs.with(|r| {
            crate::usage::visible_costs(
                state.selected.get().as_deref(),
                usage.get().as_deref(),
                r.data.as_ref(),
            )
        })
    });
    let known_total = Signal::derive(move || {
        state.snapshot.with(|s| {
            s.exact_costs
                .iter()
                .find(|d| Some(&d.date) == state.selected.get().as_ref())
                .and_then(|d| d.spend)
        })
    });
    view! {
        <Show when=move ||state.selected.with(Option::is_some)>
            <section class="card day-recap" aria-label="Récapitulatif de la journée">
                <div class="recap-heading"><h2>{move ||state.selected.get().map(|d|format!("Consommation · {d}"))}</h2><button type="button" class="close" aria-label="Fermer le récapitulatif" on:click=move |_|state.select(None)>"✕"</button></div>
                <div class="hint">"Journée locale"</div>
                {move ||state.usage.with(|r|r.loading).then(||view!{<div class="refreshing">"Chargement des détails de tokens…"</div>})}
                {move ||state.usage.with(|r|r.error.clone()).map(|e|view!{<div class="error" role="alert">{e}</div>})}
                {move ||state.costs.with(|r|r.error.clone()).filter(|_|costs.with(Option::is_none)).map(|e|view!{<div class="error" role="alert">{e}</div>})}
                <crate::daily_view::DailyBreakdown usage costs known_total />
            </section>
        </Show>
    }
}

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
pub fn HistoryCalendar(state: CalendarState) -> impl IntoView {
    let selected = state.selected;
    let history = state.snapshot;
    view! {
        <section class="card history-card">
            <h2>"Dépenses sur 30 jours"</h2>
            <div class="hint">"Dates locales · couleurs estimées depuis les agrégats UTC"</div>
            <div class="caption">"Cliquez pour les détails exacts de la journée locale."</div>
            {move || history.with(|remote| {
                let data=&remote.details;
                if data.days.is_empty() {
                    return view! { <div class="caption">"Chargement de l’historique…"</div> }.into_any();
                }
                let max = data.days.iter().filter_map(|d| d.spend).fold(0.0, f64::max);
                let total: f64 = data.days.iter().filter_map(|d| d.spend).sum();
                let incomplete = data.days.iter().any(|d| d.spend.is_none());
                let today = data.days.last().map(|d| d.date.as_str());
                let layout = cells(data);
                let columns = layout.len() / 7;
                let grid = layout.into_iter().map(|day| match day {
                    Some(day) => {
                        let date = day.date.clone();
                        let is_today = Some(day.date.as_str()) == today;
                        let tip = label(day, is_today);
                        let selected_date = date.clone();
                        let pressed_date = date.clone();
                        view! {
                            <button type="button" class=format!("history-cell {}", intensity(day.spend, max))
                                class:today=is_today class:selected=move || selected.with(|value| value.as_ref() == Some(&selected_date))
                                title=tip.clone() aria-label=tip aria-pressed=move || selected.with(|value| value.as_ref() == Some(&pressed_date))
                                on:click=move |_| state.select(Some(date.clone()))>
                                <span class="sr-only">{day.date.clone()}</span>
                            </button>
                        }.into_any()
                    }
                    None => view! { <span class="history-cell empty" aria-hidden="true"></span> }.into_any(),
                }).collect_view();
                view! {
                    <div class="history-total">{format!("≈ ${total:.2}")}<span class="caption">{if incomplete { " · estimation incomplète" } else { " · estimation sur 30 jours" }}</span></div>
                    <div class="history-layout">
                        <div class="history-weekdays" aria-hidden="true">
                            {["Lun", "Mar", "Mer", "Jeu", "Ven", "Sam", "Dim"].into_iter().map(|d| view! { <span>{d}</span> }).collect_view()}
                        </div>
                        <div class="history-grid" role="group" aria-label="Dépenses quotidiennes locales"
                            style=format!("grid-template-columns:repeat({columns},28px)")>{grid}</div>
                    </div>
                    <div class="history-legend" aria-label="Intensité croissante selon les dépenses">
                        <span>"Moins"</span>
                        {(0..=4).map(|n| view! { <span class=format!("history-swatch level-{n}")></span> }).collect_view()}
                        <span>"Plus"</span><span class="history-swatch unknown"></span><span>"Inconnu"</span>
                    </div>
                    <div class="history-detail" aria-live="polite">{move || history.with(|r| {
                        Some(&r.details).and_then(|data| {
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
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn newer_exact_cost_invalidates_old_recap_but_not_aggregate_estimate() {
        let exact = vec![HistoryDay {
            date: "2024-02-29".into(),
            spend: Some(8.0),
        }];
        let usage = shared::DailyUsage {
            date: "2024-02-29".into(),
            models: vec![shared::ModelUsage {
                spend: 6.0,
                ..Default::default()
            }],
        };
        assert!(daily_details_outdated(
            Some("2024-02-29"),
            &exact,
            Some(&usage),
            None
        ));
        assert!(!daily_details_outdated(
            Some("2024-02-28"),
            &exact,
            Some(&usage),
            None
        ));
        assert!(!daily_details_outdated(
            Some("2024-02-29"),
            &[],
            Some(&usage),
            None
        ));
    }
    #[test]
    fn local_snapshot_order_and_selection_range() {
        let current = shared::LocalHistorySnapshot {
            generation: 2,
            revision: 5,
            calendar_id: "zone-a".into(),
            ..Default::default()
        };
        let mut incoming = current.clone();
        incoming.revision = 4;
        assert!(!accept_snapshot(&current, &incoming, 2));
        incoming.revision = 6;
        assert!(accept_snapshot(&current, &incoming, 2));
        incoming.generation = 1;
        assert!(!accept_snapshot(&current, &incoming, 2));
        let days = vec![HistoryDay {
            date: "2024-02-29".into(),
            spend: Some(0.0),
        }];
        assert_eq!(
            retained_selection(Some("2024-02-29"), &days),
            Some("2024-02-29".into())
        );
        assert_eq!(retained_selection(Some("2024-02-28"), &days), None);
    }
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

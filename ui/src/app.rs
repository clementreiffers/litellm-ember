use leptos::prelude::*;
use shared::{Activity, Stats, WeekDetails};

use crate::{
    chart::{Bars, Donut, TokenSplit},
    details::{self, ActivityTab, Remote, WeekTab},
    settings::Settings,
    tauri,
};

/// Icône d'engrenage (traits arrondis, couleur héritée du bouton).
const GEAR_SVG: &str = r#"<svg class="gear" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="19.20" y1="12.00" x2="22.00" y2="12.00"/><line x1="17.09" y1="17.09" x2="19.07" y2="19.07"/><line x1="12.00" y1="19.20" x2="12.00" y2="22.00"/><line x1="6.91" y1="17.09" x2="4.93" y2="19.07"/><line x1="4.80" y1="12.00" x2="2.00" y2="12.00"/><line x1="6.91" y1="6.91" x2="4.93" y2="4.93"/><line x1="12.00" y1="4.80" x2="12.00" y2="2.00"/><line x1="17.09" y1="6.91" x2="19.07" y2="4.93"/><circle cx="12" cy="12" r="6"/><circle cx="12" cy="12" r="2.2"/></svg>"#;

/// Flèche circulaire de rafraîchissement.
const REFRESH_SVG: &str = r#"<svg class="gear" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 12a8 8 0 1 1-2.6-5.9"/><polyline points="20 3.5 20 8.5 15 8.5"/></svg>"#;

/// Croix de fermeture, même style que l'engrenage.
const CLOSE_SVG: &str = r#"<svg class="gear" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>"#;

pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1} k", n as f64 / 1e3),
        _ => format!("{:.1} M", n as f64 / 1e6),
    }
}

/// Niveau de la projection de budget : vert sous 60 %, orange sous 85 %, rouge au-delà.
pub fn budget_level(pct: f64) -> &'static str {
    if pct < 60.0 {
        "ok"
    } else if pct < 85.0 {
        "warn"
    } else {
        "crit"
    }
}

/// Couleur RGB de la jauge : vert à 0 %, orange à 50 %, rouge dès 100 %.
fn budget_color(pct: f64) -> [u8; 3] {
    let pct = pct.clamp(0.0, 100.0);
    let (start, end, progress) = if pct <= 50.0 {
        ([48.0, 209.0, 88.0], [255.0, 159.0, 10.0], pct / 50.0)
    } else {
        (
            [255.0, 159.0, 10.0],
            [255.0, 69.0, 58.0],
            (pct - 50.0) / 50.0,
        )
    };
    std::array::from_fn(|i| (start[i] + (end[i] - start[i]) * progress).round() as u8)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tab {
    Usage,
    History,
    Activity,
    Agents,
}

fn tab_for_agents(tab: Tab, count: usize) -> Tab {
    if tab == Tab::Agents && count == 0 {
        Tab::Usage
    } else {
        tab
    }
}

#[component]
pub fn App() -> impl IntoView {
    let stats = RwSignal::new(Stats::default());
    let show_settings = RwSignal::new(false);
    let scroller = NodeRef::<leptos::html::Div>::new();
    let updates = RwSignal::new(shared::UpdateStatus::default());
    let update_listeners = StoredValue::new_local(Vec::<tauri::Listener>::new());
    on_cleanup(move || {
        update_listeners.try_update_value(Vec::clear);
    });
    leptos::task::spawn_local(async move {
        let accept_update = move |incoming: shared::UpdateStatus| {
            crate::updater::accept(updates, incoming);
        };
        match tauri::on_event("update-status-changed", accept_update).await {
            Ok(guard) => {
                update_listeners.try_update_value(|listeners| listeners.push(guard));
            }
            Err(error) => {
                updates.try_update(|s| s.error = Some(error));
            }
        }
        match tauri::on_event("show-updates", move |_: ()| {
            show_settings.try_set(true);
            if let Some(element) = scroller.get_untracked() {
                element.set_scroll_top(0);
            }
        })
        .await
        {
            Ok(guard) => {
                update_listeners.try_update_value(|listeners| listeners.push(guard));
            }
            Err(error) => {
                updates.try_update(|s| s.error = Some(error));
            }
        }
        match tauri::get_update_status().await {
            Ok(status) => accept_update(status),
            Err(error) => {
                updates.try_update(|s| s.error = Some(error));
            }
        }
    });
    let tab = RwSignal::new(Tab::Usage);
    let harnesses = crate::harness::subscribe();
    Effect::new(move |_| {
        let next = tab_for_agents(tab.get(), harnesses.with(|s| s.agents.len()));
        if next != tab.get() {
            tab.set(next);
        }
    });
    // Les données des onglets secondaires vivent ici pour survivre à l'ouverture des paramètres.
    let week = RwSignal::new(Remote::<WeekDetails>::default());
    let history = RwSignal::new(Remote::<shared::HistoryDetails>::default());
    let activity = RwSignal::new(Remote::<Activity>::default());
    let selected_day = RwSignal::new(None::<String>);
    let day_usage = RwSignal::new(Remote::<shared::DailyUsage>::default());
    let today_date = RwSignal::new(crate::usage::local_today());
    // Horloge locale uniquement : aucune requête réseau supplémentaire à minuit.
    if let Ok(clock) = leptos::leptos_dom::helpers::set_interval_with_handle(
        move || {
            crate::usage::update_today(today_date);
        },
        std::time::Duration::from_secs(1),
    ) {
        on_cleanup(move || clock.clear());
    }

    let listener = StoredValue::new_local(None::<tauri::Listener>);
    on_cleanup(move || {
        listener.try_update_value(|value| {
            value.take();
        });
    });
    let accept = move |incoming: Stats| {
        today_date.try_set(crate::usage::local_today());
        stats.try_update(|current| {
            if incoming.revision >= current.revision {
                if incoming.generation != current.generation {
                    details::reset(week);
                    details::reset(history);
                    details::reset(activity);
                    details::reset(day_usage);
                }
                *current = incoming;
            }
        });
    };
    leptos::task::spawn_local(async move {
        match tauri::on_stats_updated(accept).await {
            Ok(guard) => {
                let mut guard = Some(guard);
                listener.try_update_value(|value| *value = guard.take());
            }
            Err(error) => {
                stats.try_update(|s| s.error = Some(error));
            }
        }
        if let Some(s) = tauri::get_stats().await {
            accept(s);
        }
    });
    let close_settings = Callback::new(move |_| {
        // L'endpoint ou la clé ont pu changer : les données à la demande sont à recharger.
        details::reset(week);
        details::reset(history);
        details::reset(activity);
        details::reset(day_usage);
        show_settings.set(false);
    });

    view! {
      <div class="scroller" node_ref=scroller>
        <Show when=move || show_settings.get()
            fallback=move || view! { <Main stats show_settings tab week history activity updates harnesses selected_day day_usage today_date /> }>
            <Settings on_close=close_settings updates harnesses />
        </Show>
      </div>
    }
}

#[component]
fn Main(
    stats: RwSignal<Stats>,
    show_settings: RwSignal<bool>,
    tab: RwSignal<Tab>,
    week: RwSignal<Remote<WeekDetails>>,
    history: RwSignal<Remote<shared::HistoryDetails>>,
    activity: RwSignal<Remote<Activity>>,
    updates: RwSignal<shared::UpdateStatus>,
    harnesses: RwSignal<shared::HarnessSnapshot>,
    selected_day: RwSignal<Option<String>>,
    day_usage: RwSignal<Remote<shared::DailyUsage>>,
    today_date: RwSignal<String>,
) -> impl IntoView {
    today_date.set(crate::usage::local_today());
    let generation = Signal::from(Memo::new(move |_| stats.with(|s| s.generation)));
    // Chargement à la demande à l'ouverture d'un onglet secondaire (pas au rafraîchissement périodique).
    Effect::new(move |_| {
        generation.track();
        let selected = selected_day.get();
        match tab.get() {
            Tab::History => {
                details::load(week, tauri::get_week, generation);
                details::load_history(history, generation, shared::HistoryMode::Refresh);
            }
            Tab::Activity => details::load(activity, tauri::get_activity, generation),
            Tab::Usage => {
                if let Some(date) = selected {
                    details::load(
                        day_usage,
                        move || tauri::get_day_usage(date.clone(), false),
                        generation,
                    );
                }
            }
            Tab::Agents => {}
        }
    });
    let max_budget = Signal::derive(move || stats.with(|s| s.max_budget));

    // L'état du cycle, y compris son échec, pilote l'animation.
    let refreshing = Signal::derive(move || stats.with(|s| s.refreshing));
    let force_refresh = move |_| {
        if let Some(date) = selected_day.get_untracked() {
            details::reset(day_usage);
            details::load(
                day_usage,
                move || tauri::get_day_usage(date.clone(), true),
                generation,
            );
        }
        if refreshing.get_untracked() {
            return;
        }
        leptos::task::spawn_local(async move {
            tauri::refresh_now().await;
            if week.is_disposed() {
                return;
            }
            details::invalidate(week);
            details::invalidate(activity);
            match tab.get_untracked() {
                Tab::History => {
                    details::load(week, tauri::get_week, generation);
                    details::load_history(history, generation, shared::HistoryMode::Force);
                }
                Tab::Activity => details::load(activity, tauri::get_activity, generation),
                Tab::Usage | Tab::Agents => {}
            }
        });
    };

    let tab_button = move |t: Tab, label: &'static str| {
        view! {
            <button class:active=move || tab.get() == t on:click=move |_| tab.set(t)>{label}</button>
        }
    };

    view! {
        <header>
            <div>
                <div class="app-version" title="Version d’Ember en cours d’exécution">
                    {move || updates.with(|s| if s.current_version.is_empty() {
                        "Ember".to_owned()
                    } else {
                        format!("Ember v{}", s.current_version)
                    })}
                </div>
                <div class="caption">"Coût LiteLLM de la période"</div>
                <div class="total">{move || format!("${:.2}", stats.with(|s| s.total_spend))}</div>
            </div>
            <div class="right">
                <div class="btns">
                    <button class="icon-btn" class:spinning=move || refreshing.get() title="Rafraîchir tout"
                        on:click=force_refresh inner_html=REFRESH_SVG></button>
                    <button class="icon-btn" title="Paramètres" on:click=move |_| show_settings.set(true) inner_html=GEAR_SVG></button>
                    <button class="icon-btn" title="Fermer" on:click=|_| leptos::task::spawn_local(tauri::close_window()) inner_html=CLOSE_SVG></button>
                </div>
                <div class="updated">{move || stats.with(|s| s.updated_at.clone().map(|t| format!("maj {t}")).unwrap_or_default())}</div>
            </div>
        </header>
        {move || stats.with(|s| s.error.clone()).map(|e| view! { <div class="error">{e}</div> })}
        <BudgetBar stats />

        <nav class="tabs">
            {tab_button(Tab::Usage, "Consommation")}
            {tab_button(Tab::History, "Historique")}
            {tab_button(Tab::Activity, "Activité")}
            <Show when=move || harnesses.with(|s|!s.agents.is_empty())>{tab_button(Tab::Agents, "Agents")}</Show>
        </nav>

        {move || match tab.get() {
            Tab::Agents => view! { <crate::harness::Agents snapshot=harnesses /> }.into_any(),
            Tab::Usage => view! { <UsageTab stats selected_day day_usage today_date /> }.into_any(),
            Tab::History => view! { <crate::history::HistoryCalendar history /><WeekTab week max_budget /> }.into_any(),
            Tab::Activity => view! { <ActivityTab activity /> }.into_any(),
        }}
    }
}

/// Jauge de consommation du budget de la période, colorée selon le niveau.
#[component]
fn BudgetBar(stats: RwSignal<Stats>) -> impl IntoView {
    view! {
        {move || stats.with(|s| {
            let since = s.period_start.as_deref().map(|d| format!("depuis le {}", d.get(..10).unwrap_or(d))).unwrap_or_default();
            let reset = s.budget_reset_at.as_deref().map(|d| format!("reset le {}", d.get(..10).unwrap_or(d))).unwrap_or_default();
            match s.max_budget.filter(|b| *b > 0.0) {
                Some(max) => {
                    let pct = s.period_spend / max * 100.0;
                    let [red, green, blue] = budget_color(pct);
                    view! {
                        <div class="budget" style=format!("--budget-color:{red}, {green}, {blue}")>
                            <div class="budget-head">
                                <span>{format!("${:.2} / ${max:.0}", s.period_spend)}</span>
                                <span class="budget-percentage">{format!("{pct:.0} %")}</span>
                            </div>
                            <div class="track tall">
                                <div class="fill budget-fill" style=format!("width:{:.1}%", pct.clamp(0.0, 100.0))></div>
                            </div>
                            <div class="budget-foot"><span>{since}</span><span>{reset}</span></div>
                        </div>
                    }.into_any()
                }
                None => view! {
                    <div class="hint">{format!("{since} · budget ${:.2} (aucun plafond) · {reset}", s.period_spend)}</div>
                }.into_any(),
            }
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::{budget_color, tab_for_agents, Tab};

    #[test]
    fn finishing_last_agent_returns_to_usage_without_stealing_other_tabs() {
        assert_eq!(tab_for_agents(Tab::Agents, 0), Tab::Usage);
        assert_eq!(tab_for_agents(Tab::Agents, 1), Tab::Agents);
        assert_eq!(tab_for_agents(Tab::History, 0), Tab::History);
        assert_eq!(tab_for_agents(Tab::Usage, 2), Tab::Usage);
    }

    #[test]
    fn budget_color_interpolates_through_green_orange_and_red() {
        for (pct, expected) in [
            (0.0, [48, 209, 88]),
            (25.0, [152, 184, 49]),
            (50.0, [255, 159, 10]),
            (75.0, [255, 114, 34]),
            (100.0, [255, 69, 58]),
        ] {
            assert_eq!(budget_color(pct), expected, "budget consommé : {pct} %");
        }
    }

    #[test]
    fn budget_color_clamps_negative_spending_and_overruns() {
        assert_eq!(budget_color(-25.0), budget_color(0.0));
        assert_eq!(budget_color(120.0), budget_color(100.0));
    }
}

#[component]
fn UsageTab(
    stats: RwSignal<Stats>,
    selected_day: RwSignal<Option<String>>,
    day_usage: RwSignal<Remote<shared::DailyUsage>>,
    today_date: RwSignal<String>,
) -> impl IntoView {
    let visible = Memo::new(move |_| {
        stats.with(|stats| {
            day_usage.with(|remote| {
                crate::usage::visible_usage(
                    selected_day.get().as_deref(),
                    &today_date.get(),
                    stats,
                    remote.data.as_ref(),
                )
            })
        })
    });
    let today: Signal<Vec<shared::ModelUsage>> =
        Signal::derive(move || visible.get().unwrap_or_default());
    let on_select = Callback::new(move |date: Option<String>| {
        if selected_day.get_untracked() != date {
            details::reset(day_usage);
            selected_day.set(date);
        }
    });
    let today_spend: Signal<f64> =
        Signal::derive(move || today.with(|t| t.iter().map(|u| u.spend).sum()));
    let today_tokens: Signal<u64> =
        Signal::derive(move || today.with(|t| t.iter().map(|u| u.total_tokens).sum()));
    let totals_rows: Signal<_> = Memo::new(move |_| {
        stats.with(|s| {
            s.models_total
                .iter()
                .filter(|m| m.spend > 0.0)
                .map(|m| (m.model.clone(), m.spend, format!("${:.2}", m.spend)))
                .collect::<Vec<_>>()
        })
    })
    .into();
    let token_rows: Signal<_> = Memo::new(move |_| {
        today.with(|t| {
            t.iter()
                .filter(|u| u.total_tokens > 0)
                .map(|u| {
                    (
                        u.model.clone(),
                        u.total_tokens as f64,
                        fmt_tokens(u.total_tokens),
                    )
                })
                .collect::<Vec<_>>()
        })
    })
    .into();
    let spend_rows: Signal<_> = Memo::new(move |_| {
        today.with(|t| {
            let mut v: Vec<_> = t.iter().filter(|u| u.spend > 0.0).collect();
            v.sort_by(|a, b| b.spend.total_cmp(&a.spend));
            v.into_iter()
                .map(|u| (u.model.clone(), u.spend, format!("${:.2}", u.spend)))
                .collect::<Vec<_>>()
        })
    })
    .into();
    let donut_tokens: Signal<_> = Memo::new(move |_| {
        today.with(|t| {
            t.iter()
                .filter(|u| u.total_tokens > 0)
                .map(|u| u.total_tokens as f64)
                .collect::<Vec<_>>()
        })
    })
    .into();
    let donut_label = Signal::derive(move || fmt_tokens(today_tokens.get()));

    view! {
        <section class="card">
            <h2>{move || selected_day.get().map(|d| format!("Consommation · {d}")).unwrap_or_else(|| "Aujourd'hui".into())}</h2>
            <crate::usage::DayNavigation selected=selected_day today=today_date on_select />
            <div class="hint">"Journée locale"</div>
            {move || selected_day.with(Option::is_some).then(|| view! {
                {move || day_usage.with(|r| r.loading).then(|| view! { <div class="refreshing">"Chargement de la journée…"</div> })}
                {move || day_usage.with(|r| r.error.clone()).map(|error| view! { <div class="error" role="alert">{error}</div> })}
            })}
            <Show when=move || visible.with(Option::is_some) fallback=move || view! {
                <div class="hint">"Données de cette journée indisponibles pour le moment."</div>
            }>
            {move || today.with(Vec::is_empty).then(|| view! { <div class="hint">"Aucune consommation retournée pour cette journée."</div> })}
            <div class="today">
                <Donut values=donut_tokens center_label=donut_label center_caption="tokens" />
                <div class="today-stats">
                    <div class="big">{move || format!("${:.2}", today_spend.get())}</div>
                    <div class="caption">{move || format!("{} tokens", fmt_tokens(today_tokens.get()))}</div>
                    <TokenSplit usage=today />
                </div>
            </div>
            <h3>"Tokens par modèle"</h3>
            <Bars rows=token_rows />
            <h3>"$ par modèle"</h3>
            <Bars rows=spend_rows />
            </Show>
        </section>

        <section class="card">
            <h2>"Coût de la période par modèle"</h2>
            <Bars rows=totals_rows />
            <div class="hint">"Les modèles sans prix dans LiteLLM apparaissent à $0 et sont masqués."</div>
        </section>
    }
}

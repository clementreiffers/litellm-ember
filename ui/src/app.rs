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

/// Niveau de la jauge de budget : vert sous 60 %, orange sous 85 %, rouge au-delà.
pub fn budget_level(pct: f64) -> &'static str {
    if pct < 60.0 {
        "ok"
    } else if pct < 85.0 {
        "warn"
    } else {
        "crit"
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Usage,
    Week,
    Activity,
}

#[component]
pub fn App() -> impl IntoView {
    let stats = RwSignal::new(Stats::default());
    let show_settings = RwSignal::new(false);
    let tab = RwSignal::new(Tab::Usage);
    // Les données des onglets secondaires vivent ici pour survivre à l'ouverture des paramètres.
    let week = RwSignal::new(Remote::<WeekDetails>::default());
    let activity = RwSignal::new(Remote::<Activity>::default());

    leptos::task::spawn_local(async move {
        if let Some(s) = tauri::get_stats().await {
            stats.set(s);
        }
        tauri::on_stats_updated(move |s| stats.set(s)).await;
    });

    let close_settings = Callback::new(move |_| {
        // L'endpoint ou la clé ont pu changer : les données à la demande sont à recharger.
        week.set(Remote::default());
        activity.set(Remote::default());
        show_settings.set(false);
    });

    view! {
      <div class="scroller">
        <Show when=move || show_settings.get()
            fallback=move || view! { <Main stats show_settings tab week activity /> }>
            <Settings on_close=close_settings />
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
    activity: RwSignal<Remote<Activity>>,
) -> impl IntoView {
    // Chargement à la demande à l'ouverture d'un onglet secondaire (pas au rafraîchissement périodique).
    Effect::new(move |_| match tab.get() {
        Tab::Week => details::load(week, tauri::get_week),
        Tab::Activity => details::load(activity, tauri::get_activity),
        Tab::Usage => {}
    });
    let max_budget = Signal::derive(move || stats.with(|s| s.max_budget));

    // Rafraîchissement forcé : relance le cycle du backend et recharge les onglets à la demande.
    // L'animation s'arrête quand le cycle a mis à jour les données (ou après 60 s au plus).
    let refreshing = RwSignal::new(false);
    let updated_at_click = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let updated = stats.with(|s| s.updated_at.clone());
        if refreshing.get_untracked() && updated != updated_at_click.get_untracked() {
            refreshing.set(false);
        }
    });
    let force_refresh = move |_| {
        if refreshing.get_untracked() {
            return;
        }
        updated_at_click.set(stats.with_untracked(|s| s.updated_at.clone()));
        refreshing.set(true);
        set_timeout(move || refreshing.set(false), std::time::Duration::from_secs(60));
        leptos::task::spawn_local(tauri::refresh_now());
        details::invalidate(week);
        details::invalidate(activity);
        match tab.get_untracked() {
            Tab::Week => details::load(week, tauri::get_week),
            Tab::Activity => details::load(activity, tauri::get_activity),
            Tab::Usage => {}
        }
    };

    let tab_button = move |t: Tab, label: &'static str| {
        view! {
            <button class:active=move || tab.get() == t on:click=move |_| tab.set(t)>{label}</button>
        }
    };

    view! {
        <header>
            <div>
                <div class="caption">"Coût LiteLLM de la semaine"</div>
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
            {tab_button(Tab::Week, "Semaine")}
            {tab_button(Tab::Activity, "Activité")}
        </nav>

        {move || match tab.get() {
            Tab::Usage => view! { <UsageTab stats /> }.into_any(),
            Tab::Week => view! { <WeekTab week max_budget /> }.into_any(),
            Tab::Activity => view! { <ActivityTab activity /> }.into_any(),
        }}
    }
}

/// Jauge de consommation du budget de la période, colorée selon le niveau.
#[component]
fn BudgetBar(stats: RwSignal<Stats>) -> impl IntoView {
    view! {
        {move || stats.with(|s| {
            let since = s.period_start.clone().map(|d| format!("depuis le {d}")).unwrap_or_default();
            let reset = s.budget_reset_at.clone().map(|d| format!("reset le {d}")).unwrap_or_default();
            match s.max_budget.filter(|b| *b > 0.0) {
                Some(max) => {
                    let pct = s.period_spend / max * 100.0;
                    let level = budget_level(pct);
                    view! {
                        <div class="budget">
                            <div class="budget-head">
                                <span>{format!("${:.2} / ${max:.0}", s.period_spend)}</span>
                                <span class=format!("lvl-text lvl-{level}")>{format!("{pct:.0} %")}</span>
                            </div>
                            <div class="track tall">
                                <div class=format!("fill lvl-{level}") style=format!("width:{:.1}%", pct.clamp(0.0, 100.0))></div>
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

#[component]
fn UsageTab(stats: RwSignal<Stats>) -> impl IntoView {
    let today = Signal::derive(move || stats.with(|s| s.today.clone()));
    let today_spend: Signal<f64> = Signal::derive(move || today.with(|t| t.iter().map(|u| u.spend).sum()));
    let today_tokens: Signal<u64> = Signal::derive(move || today.with(|t| t.iter().map(|u| u.total_tokens).sum()));
    let totals_rows = Signal::derive(move || {
        stats.with(|s| {
            s.models_total
                .iter()
                .filter(|m| m.spend > 0.0)
                .map(|m| (m.model.clone(), m.spend, format!("${:.2}", m.spend)))
                .collect::<Vec<_>>()
        })
    });
    let token_rows = Signal::derive(move || {
        today.with(|t| {
            t.iter()
                .filter(|u| u.total_tokens > 0)
                .map(|u| (u.model.clone(), u.total_tokens as f64, fmt_tokens(u.total_tokens)))
                .collect::<Vec<_>>()
        })
    });
    let spend_rows = Signal::derive(move || {
        today.with(|t| {
            let mut v: Vec<_> = t.iter().filter(|u| u.spend > 0.0).collect();
            v.sort_by(|a, b| b.spend.total_cmp(&a.spend));
            v.into_iter().map(|u| (u.model.clone(), u.spend, format!("${:.2}", u.spend))).collect::<Vec<_>>()
        })
    });
    let donut_tokens = Signal::derive(move || {
        today.with(|t| t.iter().filter(|u| u.total_tokens > 0).map(|u| u.total_tokens as f64).collect::<Vec<_>>())
    });
    let donut_label = Signal::derive(move || fmt_tokens(today_tokens.get()));

    view! {
        <section class="card">
            <h2>"Aujourd'hui"</h2>
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
        </section>

        <section class="card">
            <h2>"Coût de la semaine par modèle"</h2>
            <Bars rows=totals_rows />
            <div class="hint">"Les modèles sans prix dans LiteLLM apparaissent à $0 et sont masqués."</div>
        </section>
    }
}

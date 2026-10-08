//! Onglets secondaires : données chargées à la demande, avec indicateur d'attente.
use std::future::Future;

use leptos::prelude::*;
use shared::{Activity, WeekDetails};

use crate::{
    app::{budget_level, fmt_tokens},
    chart::Columns,
};

/// Les données d'un onglet ne sont pas rechargées si elles datent de moins de cette durée.
const FRESH_MS: f64 = 60_000.0;

/// État d'un appel à la demande.
#[derive(Clone)]
pub struct Remote<T> {
    pub data: Option<T>,
    pub loading: bool,
    pub error: Option<String>,
    fetched_at: f64,
}

impl<T> Default for Remote<T> {
    fn default() -> Self {
        Self { data: None, loading: false, error: None, fetched_at: 0.0 }
    }
}

/// Lance `fetch` sauf si un appel est en cours ou si les données sont encore fraîches.
pub fn load<T, F, Fut>(slot: RwSignal<Remote<T>>, fetch: F)
where
    T: Send + Sync + 'static,
    F: FnOnce() -> Fut + 'static,
    Fut: Future<Output = Result<T, String>> + 'static,
{
    let skip = slot.with_untracked(|r| r.loading || (r.data.is_some() && js_sys::Date::now() - r.fetched_at < FRESH_MS));
    if skip {
        return;
    }
    slot.update(|r| {
        r.loading = true;
        r.error = None;
    });
    leptos::task::spawn_local(async move {
        let result = fetch().await;
        slot.update(|r| {
            r.loading = false;
            match result {
                Ok(d) => {
                    r.data = Some(d);
                    r.fetched_at = js_sys::Date::now();
                }
                Err(e) => r.error = Some(e),
            }
        });
    });
}

/// Bandeau d'état commun : attente (première charge), actualisation (données déjà là) ou erreur.
#[component]
fn Status(loading: Signal<bool>, has_data: Signal<bool>, error: Signal<Option<String>>) -> impl IntoView {
    view! {
        {move || (loading.get() && !has_data.get()).then(|| view! {
            <div class="pending"><span class="spinner"></span>"Récupération des informations en cours…"</div>
        })}
        {move || (loading.get() && has_data.get()).then(|| view! {
            <div class="refreshing"><span class="spinner small"></span>"actualisation…"</div>
        })}
        {move || error.get().map(|e| view! { <div class="error">{e}</div> })}
    }
}

fn fmt_ms(ms: Option<f64>) -> String {
    match ms {
        None => "—".into(),
        Some(v) if v >= 1000.0 => format!("{:.1} s", v / 1000.0),
        Some(v) => format!("{v:.0} ms"),
    }
}

fn fmt_cost(v: f64) -> String {
    if v >= 1.0 { format!("${v:.2}") } else { format!("${v:.3}") }
}

/// « 2026-10-08 » -> « 08/10 ».
fn short_date(iso: &str) -> String {
    match (iso.get(8..10), iso.get(5..7)) {
        (Some(d), Some(m)) => format!("{d}/{m}"),
        _ => iso.to_string(),
    }
}

#[component]
pub fn WeekTab(week: RwSignal<Remote<WeekDetails>>, max_budget: Signal<Option<f64>>) -> impl IntoView {
    let loading = Signal::derive(move || week.with(|r| r.loading));
    let has_data = Signal::derive(move || week.with(|r| r.data.is_some()));
    let error = Signal::derive(move || week.with(|r| r.error.clone()));
    let days = Signal::derive(move || week.with(|r| r.data.as_ref().map(|d| d.days.clone()).unwrap_or_default()));

    let cols = Signal::derive(move || {
        days.get()
            .into_iter()
            .map(|d| (short_date(&d.date), d.spend, format!("{} : ${:.2}", d.date, d.spend)))
            .collect::<Vec<_>>()
    });
    let today_idx = Signal::derive(move || days.with(|d| d.len().checked_sub(1)));

    view! {
        <Status loading has_data error />
        {move || week.with(|r| r.data.clone()).map(|w| {
            let n = w.days.len();
            let today = w.days.last().map(|d| d.spend).unwrap_or(0.0);
            let yesterday = n.checked_sub(2).and_then(|i| w.days.get(i)).map(|d| d.spend);
            let delta = yesterday.filter(|y| *y > 0.0).map(|y| (today - y) / y * 100.0);
            let projection = w.projection;
            view! {
                <section class="card">
                    <h2>"Projection de fin de période"</h2>
                    {match projection {
                        Some(p) => {
                            let pct = max_budget.get().filter(|b| *b > 0.0).map(|b| p / b * 100.0);
                            let level = pct.map(budget_level).unwrap_or("ok");
                            view! {
                                <div class=format!("big lvl-text lvl-{level}")>{format!("${p:.2}")}</div>
                                <div class="caption">
                                    {match pct {
                                        Some(pct) => format!("au rythme actuel, soit {pct:.0} % du budget"),
                                        None => "au rythme actuel".to_string(),
                                    }}
                                </div>
                            }.into_any()
                        }
                        None => view! { <div class="caption">"Pas assez de recul pour projeter (moins de 6 h dans la période)."</div> }.into_any(),
                    }}
                </section>
                <section class="card">
                    <h2>"Aujourd'hui vs hier"</h2>
                    <div class="compare">
                        <div><div class="caption">"Hier"</div><div class="big">{yesterday.map(|y| format!("${y:.2}")).unwrap_or("—".into())}</div></div>
                        <div><div class="caption">"Aujourd'hui"</div><div class="big">{format!("${today:.2}")}</div></div>
                        {delta.map(|d| {
                            let class = if d > 0.0 { "delta up" } else { "delta down" };
                            let arrow = if d > 0.0 { "▲" } else { "▼" };
                            view! { <div class=class>{format!("{arrow} {:.0} %", d.abs())}</div> }
                        })}
                    </div>
                    <div class="hint">"Jours en UTC."</div>
                </section>
                <section class="card">
                    <h2>"Évolution sur la période"</h2>
                    <Columns cols highlight=today_idx />
                </section>
            }
        })}
    }
}

#[component]
pub fn ActivityTab(activity: RwSignal<Remote<Activity>>) -> impl IntoView {
    let loading = Signal::derive(move || activity.with(|r| r.loading));
    let has_data = Signal::derive(move || activity.with(|r| r.data.is_some()));
    let error = Signal::derive(move || activity.with(|r| r.error.clone()));
    let hourly = Signal::derive(move || {
        activity.with(|r| {
            r.data
                .as_ref()
                .map(|a| {
                    a.hourly
                        .iter()
                        .enumerate()
                        .map(|(h, b)| {
                            let label = if h % 6 == 0 { format!("{h}h") } else { String::new() };
                            (label, b.requests as f64, format!("{h}h : {} req · ${:.2}", b.requests, b.spend))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let no_highlight = Signal::derive(|| None);

    view! {
        <Status loading has_data error />
        {move || activity.with(|r| r.data.clone()).map(|a| {
            let error_pct = if a.requests > 0 { a.errors as f64 / a.requests as f64 * 100.0 } else { 0.0 };
            let cache = a.cache_rate.map(|c| format!("{:.0} %", c * 100.0)).unwrap_or("non utilisé".into());
            view! {
                <section class="card">
                    <h2>"Aujourd'hui en un coup d'œil"</h2>
                    <div class="kpis">
                        <div class="kpi"><div class="caption">"Requêtes"</div><div class="kpi-val">{a.requests}</div></div>
                        <div class="kpi"><div class="caption">"Coût moyen / req"</div><div class="kpi-val">{fmt_cost(a.avg_cost)}</div></div>
                        <div class="kpi"><div class="caption">"Tokens moyens / req"</div><div class="kpi-val">{fmt_tokens(a.avg_tokens as u64)}</div></div>
                        <div class="kpi"><div class="caption">"Taux d'erreur"</div>
                            <div class=format!("kpi-val {}", if error_pct >= 10.0 { "crit" } else if error_pct >= 3.0 { "warn" } else { "" })>
                                {format!("{error_pct:.1} %")}
                            </div>
                            <div class="caption">{format!("{} échec(s)", a.errors)}</div>
                        </div>
                        <div class="kpi"><div class="caption">"Cache"</div><div class="kpi-val">{cache}</div></div>
                        <div class="kpi"><div class="caption">"Latence p50 / p95"</div>
                            <div class="kpi-val">{fmt_ms(a.latency_p50_ms)}</div>
                            <div class="caption">{format!("p95 {} · 1er token {}", fmt_ms(a.latency_p95_ms), fmt_ms(a.ttft_p50_ms))}</div>
                        </div>
                    </div>
                </section>
                <section class="card">
                    <h2>"Par modèle"</h2>
                    <table class="models">
                        <tr class="th"><td>"modèle"</td><td class="num">"req"</td><td class="num">"$/req"</td><td class="num">"p50"</td><td class="num">"p95"</td><td class="num">"err"</td></tr>
                        {a.per_model.iter().map(|m| {
                            let err_class = if m.errors > 0 { "num crit" } else { "num" };
                            view! {
                            <tr>
                                <td class="name">{m.model.clone()}</td>
                                <td class="num">{m.requests}</td>
                                <td class="num">{fmt_cost(m.avg_cost)}</td>
                                <td class="num">{fmt_ms(m.latency_p50_ms)}</td>
                                <td class="num">{fmt_ms(m.latency_p95_ms)}</td>
                                <td class=err_class>{m.errors}</td>
                            </tr>
                        }}).collect_view()}
                    </table>
                </section>
                <section class="card">
                    <h2>"Rythme horaire (requêtes)"</h2>
                    <Columns cols=hourly highlight=no_highlight compact=true />
                    <div class="hint">"Heure locale. Survolez une barre pour le détail."</div>
                </section>
                {a.top_request.clone().map(|t| view! {
                    <section class="card">
                        <h2>"Requête la plus chère"</h2>
                        <div class="big">{format!("${:.2}", t.spend)}</div>
                        <div class="caption">{format!("{} · {} tokens · à {}", t.model, fmt_tokens(t.total_tokens), t.time)}</div>
                    </section>
                })}
            }
        })}
    }
}

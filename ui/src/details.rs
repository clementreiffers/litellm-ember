//! Onglets secondaires : données chargées à la demande, avec indicateur d'attente.
use std::future::Future;

use leptos::prelude::*;
use shared::{Activity, Detail, WeekDetails};

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
    request_id: u64,
    pending: bool,
}

impl<T> Default for Remote<T> {
    fn default() -> Self {
        Self {
            data: None,
            loading: false,
            error: None,
            fetched_at: 0.0,
            request_id: 0,
            pending: false,
        }
    }
}

/// Transitions sans horloge JS, testables sur la cible native.
impl<T> Remote<T> {
    fn begin(&mut self, now: f64) -> Option<u64> {
        if self.loading
            || (self.data.is_some() && self.fetched_at > 0.0 && now - self.fetched_at < FRESH_MS)
        {
            return None;
        }
        self.loading = true;
        self.error = None;
        self.request_id += 1;
        Some(self.request_id)
    }
    fn complete(
        &mut self,
        request_id: u64,
        generation: u64,
        current_generation: u64,
        result: Result<Detail<T>, String>,
        now: f64,
    ) -> bool {
        if self.request_id != request_id {
            return false;
        }
        self.loading = false;
        let reload = std::mem::take(&mut self.pending);
        if generation != current_generation {
            return false;
        }
        match result {
            Ok(detail) if detail.generation == generation => {
                self.data = Some(detail.data);
                self.fetched_at = if reload { 0.0 } else { now };
            }
            Ok(_) => self.error = Some("La configuration a changé ; rechargez cet onglet".into()),
            Err(error) => self.error = Some(error),
        }
        reload
    }
    fn invalidate(&mut self) {
        self.fetched_at = 0.0;
        self.pending |= self.loading;
    }
    fn reset(&mut self) {
        *self = Self {
            request_id: self.request_id + 1,
            ..Default::default()
        };
    }
}

/// Lance `fetch` sauf si un appel est en cours ou si les données sont encore fraîches.
pub fn load<T, F, Fut>(slot: RwSignal<Remote<T>>, fetch: F, generation: Signal<u64>)
where
    T: Send + Sync + 'static,
    F: Fn() -> Fut + Clone + 'static,
    Fut: Future<Output = Result<Detail<T>, String>> + 'static,
{
    let Some(Some(request_id)) = slot.try_update(|r| r.begin(js_sys::Date::now())) else {
        return;
    };
    let requested_generation = generation.get_untracked();
    leptos::task::spawn_local(async move {
        let result = fetch().await;
        let mut reload = false;
        slot.try_update(|r| {
            if let Some(current) = generation.try_get_untracked() {
                reload = r.complete(
                    request_id,
                    requested_generation,
                    current,
                    result,
                    js_sys::Date::now(),
                );
            }
        });
        if reload && slot.try_with_untracked(|_| ()).is_some() {
            load(slot, fetch, generation);
        }
    });
}

/// Publie d'abord le cache local, puis actualise sans masquer les cases déjà connues.
pub fn load_history(
    slot: RwSignal<Remote<shared::HistoryDetails>>,
    generation: Signal<u64>,
    mode: shared::HistoryMode,
) {
    let Some(Some(id)) = slot.try_update(|r| {
        if mode == shared::HistoryMode::Force {
            r.invalidate();
        }
        // La fraîcheur est centralisée dans le backend, y compris au changement de jour UTC.
        r.fetched_at = 0.0;
        r.begin(js_sys::Date::now())
    }) else {
        return;
    };
    let requested_generation = generation.get_untracked();
    leptos::task::spawn_local(async move {
        if let Ok(cached) = crate::tauri::get_history(shared::HistoryMode::Cached).await {
            slot.try_update(|r| {
                if r.request_id == id
                    && generation.try_get_untracked() == Some(requested_generation)
                    && cached.generation == requested_generation
                {
                    r.data = Some(cached.data);
                }
            });
        }
        let result = crate::tauri::get_history(mode).await;
        let mut reload = false;
        slot.try_update(|r| {
            if let Some(current) = generation.try_get_untracked() {
                reload = r.complete(
                    id,
                    requested_generation,
                    current,
                    result,
                    js_sys::Date::now(),
                );
            }
        });
        if reload && slot.try_with_untracked(|_| ()).is_some() {
            load_history(slot, generation, shared::HistoryMode::Force);
        }
    });
}

pub fn invalidate<T: Send + Sync + 'static>(slot: RwSignal<Remote<T>>) {
    slot.update(Remote::invalidate);
}
pub fn reset<T: Send + Sync + 'static>(slot: RwSignal<Remote<T>>) {
    slot.update(Remote::reset);
}

/// Bandeau d'état commun : attente (première charge), actualisation (données déjà là) ou erreur.
#[component]
fn Status(
    loading: Signal<bool>,
    has_data: Signal<bool>,
    error: Signal<Option<String>>,
) -> impl IntoView {
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
    if v >= 1.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.3}")
    }
}

/// « 2026-10-08 » -> « 08/10 ».
fn short_date(iso: &str) -> String {
    match (iso.get(8..10), iso.get(5..7)) {
        (Some(d), Some(m)) => format!("{d}/{m}"),
        _ => iso.to_string(),
    }
}

#[component]
pub fn WeekTab(
    week: RwSignal<Remote<WeekDetails>>,
    max_budget: Signal<Option<f64>>,
) -> impl IntoView {
    let loading = Signal::derive(move || week.with(|r| r.loading));
    let has_data = Signal::derive(move || week.with(|r| r.data.is_some()));
    let error = Signal::derive(move || week.with(|r| r.error.clone()));
    let days = Signal::derive(move || {
        week.with(|r| r.data.as_ref().map(|d| d.days.clone()).unwrap_or_default())
    });

    let cols = Signal::derive(move || {
        days.get()
            .into_iter()
            .map(|d| {
                (
                    short_date(&d.date),
                    d.spend,
                    format!("{} : ${:.2}", d.date, d.spend),
                )
            })
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
                            let label = if h % 6 == 0 {
                                format!("{h}h")
                            } else {
                                String::new()
                            };
                            (
                                label,
                                b.requests as f64,
                                format!("{h}h : {} req · ${:.2}", b.requests, b.spend),
                            )
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
            let cache = a.cache_rate.map(|c| format!("{:.0} %", c * 100.0)).unwrap_or("Indisponible".into());
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
                        <div class="kpi"><div class="caption">"Cache de réponses LiteLLM"</div><div class="kpi-val">{cache}</div></div>
                        <div class="kpi"><div class="caption">"Latence p50 / p95"</div>
                            <div class="kpi-val">{fmt_ms(a.latency_p50_ms)}</div>
                            <div class="caption">{format!("p95 {} · 1er token {}", fmt_ms(a.latency_p95_ms), fmt_ms(a.ttft_p50_ms))}</div>
                        </div>
                    </div>
                </section>
                <PromptCacheCard summary=a.prompt_cache.clone() models=a.per_model.clone() />
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

fn prompt_cache_rate(usage: &shared::PromptCacheUsage) -> String {
    usage
        .rate()
        .map(|rate| format!("{:.1} %", rate * 100.0))
        .unwrap_or_else(|| "—".into())
}

fn prompt_cache_message(usage: &shared::PromptCacheUsage) -> &'static str {
    if usage.eligible_requests == 0 {
        "Aucune requête éligible aujourd’hui"
    } else if usage.reported_requests == 0 {
        "Données de cache de prompts indisponibles"
    } else {
        "Sur les requêtes renseignées"
    }
}

#[component]
fn PromptCacheCard(
    summary: shared::PromptCacheUsage,
    mut models: Vec<shared::ModelActivity>,
) -> impl IntoView {
    models.retain(|model| model.prompt_cache.eligible_requests > 0);
    models.sort_by(|a, b| {
        b.prompt_cache
            .cached_tokens
            .cmp(&a.prompt_cache.cached_tokens)
            .then(a.model.cmp(&b.model))
    });
    let reported = summary.reported_requests > 0;
    let rate = summary.rate();
    view! {
        <section class="card prompt-cache">
            <h2>"Cache de prompts · Aujourd’hui"</h2>
            <p class="caption">{prompt_cache_message(&summary)}</p>
            {reported.then(|| view! {
                <div class="prompt-cache-totals">
                    <div><div class="kpi-val">{fmt_tokens(summary.cached_tokens)}</div><div class="caption">"Tokens réutilisés"</div></div>
                    <div><div class="kpi-val">{prompt_cache_rate(&summary)}</div><div class="caption">"Des tokens d’entrée réutilisés"</div></div>
                </div>
            })}
            {rate.map(|rate| view! {
                <progress class="prompt-cache-progress" max="1" value=rate aria-label="Part des tokens d’entrée réutilisés" />
            })}
            <p class="caption">{format!("Données disponibles pour {} requêtes sur {}", summary.reported_requests, summary.eligible_requests)}</p>
            {(!models.is_empty()).then(|| view! {
                <table class="models">
                    <thead><tr class="th"><th>"Modèle"</th><th class="num">"Réutilisés"</th><th class="num">"Part"</th><th class="num">"Requêtes renseignées"</th></tr></thead>
                    <tbody>{models.into_iter().map(|model| {
                        let usage = model.prompt_cache;
                        let title = model.model.clone();
                        let tokens = if usage.reported_requests > 0 { fmt_tokens(usage.cached_tokens) } else { "—".into() };
                        view! {
                            <tr>
                                <td class="name" title=title>{model.model}</td>
                                <td class="num">{tokens}</td>
                                <td class="num">{prompt_cache_rate(&usage)}</td>
                                <td class="num">{format!("{} / {}", usage.reported_requests, usage.eligible_requests)}</td>
                            </tr>
                        }
                    }).collect_view()}</tbody>
                </table>
            })}
            <p class="hint">"Tokens d’entrée relus depuis le cache des fournisseurs. Échecs et réponses servies par le cache LiteLLM exclus. Les données absentes ne comptent pas comme zéro."</p>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_cache_labels_distinguish_missing_data_from_zero_usage() {
        let mut usage = shared::PromptCacheUsage::default();
        assert_eq!(
            prompt_cache_message(&usage),
            "Aucune requête éligible aujourd’hui"
        );
        usage.eligible_requests = 2;
        assert_eq!(
            prompt_cache_message(&usage),
            "Données de cache de prompts indisponibles"
        );
        assert_eq!(prompt_cache_rate(&usage), "—");
        usage.reported_requests = 1;
        assert_eq!(prompt_cache_message(&usage), "Sur les requêtes renseignées");
        assert_eq!(prompt_cache_rate(&usage), "—");
        usage.input_tokens = 100;
        assert_eq!(prompt_cache_rate(&usage), "0.0 %");
        usage.cached_tokens = 75;
        assert_eq!(prompt_cache_rate(&usage), "75.0 %");
    }

    fn result(value: u32, generation: u64) -> Result<Detail<u32>, String> {
        Ok(Detail {
            generation,
            data: value,
        })
    }
    #[test]
    fn old_response_cannot_overwrite_a_new_request_after_reset() {
        let mut remote = Remote::default();
        let old = remote.begin(100.0).unwrap();
        remote.reset();
        let new = remote.begin(200.0).unwrap();
        assert!(!remote.complete(old, 1, 1, result(10, 1), 300.0));
        assert!(remote.loading);
        assert_eq!(remote.data, None);
        remote.complete(new, 1, 1, result(20, 1), 400.0);
        assert_eq!(remote.data, Some(20));
    }
    #[test]
    fn invalidations_during_loading_queue_exactly_one_reload() {
        let mut remote = Remote::default();
        let request = remote.begin(100.0).unwrap();
        remote.invalidate();
        remote.invalidate();
        assert!(remote.begin(200.0).is_none());
        assert!(remote.complete(request, 1, 1, result(10, 1), 300.0));
        let next = remote.begin(300.0).unwrap();
        assert!(!remote.complete(next, 1, 1, result(20, 1), 400.0));
        assert!(remote.begin(500.0).is_none());
        assert!(remote.begin(60_400.0).is_some());
    }
    #[test]
    fn changed_generation_and_failed_requests_leave_existing_data_untouched() {
        let mut remote = Remote {
            data: Some(5),
            ..Default::default()
        };
        let request = remote.begin(100.0).unwrap();
        remote.complete(request, 1, 2, result(9, 1), 200.0);
        assert_eq!(remote.data, Some(5));
        let request = remote.begin(300.0).unwrap();
        remote.complete(request, 2, 2, Err("offline".into()), 400.0);
        assert_eq!(remote.data, Some(5));
        assert!(!remote.loading);
        assert!(remote.error.is_some());
    }
}

use leptos::prelude::*;
use shared::Stats;

use crate::{
    chart::{Bars, Donut, TokenSplit},
    tauri,
};

pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1} k", n as f64 / 1e3),
        _ => format!("{:.1} M", n as f64 / 1e6),
    }
}

#[component]
pub fn App() -> impl IntoView {
    let stats = RwSignal::new(Stats::default());

    leptos::task::spawn_local(async move {
        if let Some(s) = tauri::get_stats().await {
            stats.set(s);
        }
        tauri::on_stats_updated(move |s| stats.set(s)).await;
    });

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
        <header>
            <div>
                <div class="caption">"Coût LiteLLM de la semaine"</div>
                <div class="total">{move || format!("${:.2}", stats.with(|s| s.total_spend))}</div>
            </div>
            <div class="right">
                <button class="close" on:click=|_| leptos::task::spawn_local(tauri::close_window())>"✕"</button>
                <div class="updated">{move || stats.with(|s| s.updated_at.clone().map(|t| format!("maj {t}")).unwrap_or_default())}</div>
            </div>
        </header>
        {move || stats.with(|s| s.error.clone()).map(|e| view! { <div class="error">{e}</div> })}
        <div class="hint">
            {move || stats.with(|s| {
                let budget = s.max_budget.map(|b| format!(" / ${b:.0}")).unwrap_or_default();
                let reset = s.budget_reset_at.clone().map(|d| format!(" · reset le {d}")).unwrap_or_default();
                let since = s.period_start.clone().map(|d| format!("depuis le {d} · ")).unwrap_or_default();
                format!("{since}budget ${:.2}{budget}{reset}", s.period_spend)
            })}
        </div>

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

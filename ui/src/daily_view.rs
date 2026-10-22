//! Figures quotidiennes communes à Consommation et au récapitulatif du calendrier.
use crate::{
    app::fmt_tokens,
    chart::{Bars, Donut, TokenSplit},
};
use leptos::prelude::*;
use shared::{ModelCost, ModelUsage};

#[component]
pub fn DailyBreakdown(
    usage: Signal<Option<Vec<ModelUsage>>>,
    costs: Signal<Option<Vec<ModelCost>>>,
    known_total: Signal<Option<f64>>,
) -> impl IntoView {
    let total = Signal::derive(move || {
        usage.with(|u| {
            costs.with(|c| crate::usage::daily_total(u.as_deref(), c.as_deref(), known_total.get()))
        })
    });
    let rows = Signal::derive(move || usage.get().unwrap_or_default());
    let tokens =
        Signal::derive(move || rows.with(|r| r.iter().map(|m| m.total_tokens).sum::<u64>()));
    let donut = Signal::derive(move || {
        rows.with(|r| {
            r.iter()
                .filter(|m| m.total_tokens > 0)
                .map(|m| m.total_tokens as f64)
                .collect()
        })
    });
    let label = Signal::derive(move || fmt_tokens(tokens.get()));
    let token_rows = Signal::derive(move || {
        rows.with(|r| {
            r.iter()
                .filter(|m| m.total_tokens > 0)
                .map(|m| {
                    (
                        m.model.clone(),
                        m.total_tokens as f64,
                        fmt_tokens(m.total_tokens),
                    )
                })
                .collect()
        })
    });
    let spend_rows = Signal::derive(move || {
        let mut values = usage
            .with(|u| {
                u.as_ref().map(|r| {
                    r.iter()
                        .map(|m| ModelCost {
                            model: m.model.clone(),
                            spend: m.spend,
                        })
                        .collect::<Vec<_>>()
                })
            })
            .or_else(|| costs.get())
            .unwrap_or_default();
        values.sort_by(|a, b| b.spend.total_cmp(&a.spend));
        values
            .into_iter()
            .filter(|m| m.spend > 0.0)
            .map(|m| (m.model, m.spend, format!("${:.2}", m.spend)))
            .collect()
    });
    view! {
        <Show when=move ||total.with(Option::is_some) fallback=||view!{<div class="hint">"Données de cette journée indisponibles pour le moment."</div>}>
            {move ||usage.with(|u|u.as_ref().is_some_and(Vec::is_empty)).then(||view!{<div class="hint">"Aucune consommation retournée pour cette journée."</div>})}
            <div class="today">
                <Show when=move ||usage.with(Option::is_some)><Donut values=donut center_label=label center_caption="tokens" /></Show>
                <div class="today-stats">
                    <div class="big">{move ||format!("${:.2}",total.get().unwrap_or(0.0))}</div>
                    <Show when=move ||usage.with(Option::is_some) fallback=||view!{<div class="caption">"Coûts disponibles · détails de tokens en attente"</div>}>
                        <div class="caption">{move ||format!("{} tokens",fmt_tokens(tokens.get()))}</div>
                        <TokenSplit usage=rows />
                        <div class="caption token-counts">{move ||rows.with(|r|format!("Entrée {} · sortie {}",fmt_tokens(r.iter().map(|m|m.prompt_tokens).sum()),fmt_tokens(r.iter().map(|m|m.completion_tokens).sum())))}</div>
                    </Show>
                </div>
            </div>
            <Show when=move ||usage.with(Option::is_some)><h3>"Tokens par modèle"</h3><Bars rows=token_rows /></Show>
            <Show when=move ||costs.with(Option::is_some)||usage.with(Option::is_some)><h3>"$ par modèle"</h3><Bars rows=spend_rows /></Show>
        </Show>
    }
}

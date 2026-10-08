//! Graphiques 100 % Rust (HTML/CSS + SVG), sans JavaScript externe.
use leptos::prelude::*;
use shared::ModelUsage;

const PALETTE: [(&str, &str); 6] = [
    ("#7c5cff", "#b49cff"),
    ("#00c2a8", "#6ff2dc"),
    ("#ff7a59", "#ffb199"),
    ("#2f9bff", "#8cc8ff"),
    ("#ffc531", "#ffe08a"),
    ("#ff4d8d", "#ff9dc0"),
];

pub fn color(i: usize) -> (&'static str, &'static str) {
    PALETTE[i % PALETTE.len()]
}

/// Barres horizontales en dégradé, une ligne par modèle.
#[component]
pub fn Bars(
    /// (modèle, valeur, valeur affichée)
    rows: Signal<Vec<(String, f64, String)>>,
) -> impl IntoView {
    view! {
        <div class="bars">
            {move || {
                let rows = rows.get();
                let max = rows.iter().map(|r| r.1).fold(0.0_f64, f64::max).max(f64::MIN_POSITIVE);
                rows.into_iter()
                    .enumerate()
                    .map(|(i, (name, value, label))| {
                        let (c1, c2) = color(i);
                        let pct = (value / max * 100.0).max(if value > 0.0 { 1.5 } else { 0.0 });
                        view! {
                            <div class="bar-row">
                                <div class="bar-head"><span class="name">{name}</span><span class="val">{label}</span></div>
                                <div class="track">
                                    <div class="fill" style=format!("width:{pct:.1}%;background:linear-gradient(90deg,{c1},{c2})")></div>
                                </div>
                            </div>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}

/// Donut SVG : part de chaque modèle dans la métrique choisie.
#[component]
pub fn Donut(
    values: Signal<Vec<f64>>,
    center_label: Signal<String>,
    center_caption: &'static str,
) -> impl IntoView {
    const R: f64 = 42.0;
    let circ = 2.0 * std::f64::consts::PI * R;
    view! {
        <svg class="donut" viewBox="0 0 120 120">
            <circle cx="60" cy="60" r=R fill="none" stroke="#2c2c2e" stroke-width="14" />
            {move || {
                let vals = values.get();
                let total: f64 = vals.iter().sum();
                let mut offset = 0.0;
                vals.iter()
                    .enumerate()
                    .filter(|(_, v)| **v > 0.0 && total > 0.0)
                    .map(|(i, v)| {
                        let len = v / total * circ;
                        let dash = format!("{:.2} {:.2}", (len - 1.5).max(0.5), circ);
                        let off = -offset;
                        offset += len;
                        view! {
                            <circle cx="60" cy="60" r=R fill="none" stroke=color(i).0 stroke-width="14"
                                stroke-dasharray=dash stroke-dashoffset=format!("{off:.2}")
                                transform="rotate(-90 60 60)" stroke-linecap="butt" />
                        }
                    })
                    .collect_view()
            }}
            <text x="60" y="58" text-anchor="middle" class="donut-val">{move || center_label.get()}</text>
            <text x="60" y="74" text-anchor="middle" class="donut-cap">{center_caption}</text>
        </svg>
    }
}

/// Barre empilée prompt / completion pour les tokens du jour.
#[component]
pub fn TokenSplit(usage: Signal<Vec<ModelUsage>>) -> impl IntoView {
    view! {
        {move || {
            let u = usage.get();
            let p: u64 = u.iter().map(|m| m.prompt_tokens).sum();
            let c: u64 = u.iter().map(|m| m.completion_tokens).sum();
            let total = (p + c).max(1) as f64;
            let pp = p as f64 / total * 100.0;
            view! {
                <div class="split">
                    <div style=format!("width:{pp:.1}%;background:linear-gradient(90deg,#7c5cff,#b49cff)")></div>
                    <div style=format!("width:{:.1}%;background:linear-gradient(90deg,#00c2a8,#6ff2dc)", 100.0 - pp)></div>
                </div>
                <div class="legend">
                    <span><i style="background:#7c5cff"></i>"prompt"</span>
                    <span><i style="background:#00c2a8"></i>"completion"</span>
                </div>
            }
        }}
    }
}

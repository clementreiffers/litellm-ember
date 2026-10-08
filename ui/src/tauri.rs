//! Pont vers l'API JS de Tauri (`withGlobalTauri: true`).
use shared::Stats;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], catch)]
    async fn listen(event: &str, handler: &Closure<dyn FnMut(JsValue)>) -> Result<JsValue, JsValue>;
}

pub async fn get_stats() -> Option<Stats> {
    let value = invoke("get_stats").await.ok()?;
    serde_wasm_bindgen::from_value(value).ok()
}

/// Appelle `on_update` à chaque événement `stats-updated`.
pub async fn on_stats_updated(on_update: impl Fn(Stats) + 'static) {
    let handler = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
        let payload = js_sys::Reflect::get(&event, &JsValue::from_str("payload")).unwrap_or(JsValue::NULL);
        if let Ok(stats) = serde_wasm_bindgen::from_value(payload) {
            on_update(stats);
        }
    });
    let _ = listen("stats-updated", &handler).await;
    // Le listener doit vivre aussi longtemps que la fenêtre.
    handler.forget();
}

pub async fn close_window() {
    let _ = invoke("close_window").await;
}

//! Pont vers l'API JS de Tauri (`withGlobalTauri: true`).
use serde::Serialize;
use shared::{Activity, SettingsInput, SettingsView, Stats, WeekDetails};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = "invoke", catch)]
    async fn invoke_with(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

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

pub async fn get_settings() -> Option<SettingsView> {
    let value = invoke("get_settings").await.ok()?;
    serde_wasm_bindgen::from_value(value).ok()
}

#[derive(Serialize)]
struct SaveArgs<'a> {
    input: &'a SettingsInput,
}

/// Enregistre les paramètres ; renvoie le message d'erreur du backend en cas de refus.
pub async fn save_settings(input: &SettingsInput) -> Result<SettingsView, String> {
    let args = serde_wasm_bindgen::to_value(&SaveArgs { input }).map_err(|e| e.to_string())?;
    match invoke_with("save_settings", args).await {
        Ok(v) => serde_wasm_bindgen::from_value(v).map_err(|e| e.to_string()),
        Err(e) => Err(e.as_string().unwrap_or_else(|| "Échec de l'enregistrement".into())),
    }
}

async fn invoke_as<T: serde::de::DeserializeOwned>(cmd: &str) -> Result<T, String> {
    match invoke(cmd).await {
        Ok(v) => serde_wasm_bindgen::from_value(v).map_err(|e| e.to_string()),
        Err(e) => Err(e.as_string().unwrap_or_else(|| "Échec de la récupération".into())),
    }
}

pub async fn get_week() -> Result<WeekDetails, String> {
    invoke_as("get_week").await
}

pub async fn get_activity() -> Result<Activity, String> {
    invoke_as("get_activity").await
}

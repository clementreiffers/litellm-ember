//! Pont vers l'API JS de Tauri (`withGlobalTauri: true`).
use serde::Serialize;
use shared::{Activity, Detail, SettingsInput, SettingsView, Stats, WeekDetails};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = "invoke", catch)]
    async fn invoke_with(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], catch)]
    async fn listen(event: &str, handler: &Closure<dyn FnMut(JsValue)>)
        -> Result<JsValue, JsValue>;
}

pub async fn get_stats() -> Option<Stats> {
    let value = invoke("get_stats").await.ok()?;
    serde_wasm_bindgen::from_value(value).ok()
}

/// Le guard désinscrit le listener avant de libérer sa closure Wasm.
pub struct Listener {
    unlisten: js_sys::Function,
    _handler: Closure<dyn FnMut(JsValue)>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.unlisten.call0(&JsValue::NULL);
    }
}
pub async fn on_stats_updated(on_update: impl Fn(Stats) + 'static) -> Result<Listener, String> {
    let handler = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
        let payload =
            js_sys::Reflect::get(&event, &JsValue::from_str("payload")).unwrap_or(JsValue::NULL);
        if let Ok(stats) = serde_wasm_bindgen::from_value(payload) {
            on_update(stats);
        }
    });
    let unlisten = listen("stats-updated", &handler)
        .await
        .map_err(|_| "Abonnement aux statistiques impossible")?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| "Réponse d'abonnement invalide")?;
    Ok(Listener {
        unlisten,
        _handler: handler,
    })
}

pub async fn close_window() {
    let _ = invoke("close_window").await;
}

pub async fn get_settings() -> Result<SettingsView, String> {
    invoke_as("get_settings").await
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
        Err(e) => Err(e
            .as_string()
            .unwrap_or_else(|| "Échec de l'enregistrement".into())),
    }
}

async fn invoke_as<T: serde::de::DeserializeOwned>(cmd: &str) -> Result<T, String> {
    match invoke(cmd).await {
        Ok(v) => serde_wasm_bindgen::from_value(v).map_err(|e| e.to_string()),
        Err(e) => Err(e
            .as_string()
            .unwrap_or_else(|| "Échec de la récupération".into())),
    }
}

pub async fn get_week() -> Result<Detail<WeekDetails>, String> {
    invoke_as("get_week").await
}

pub async fn get_activity() -> Result<Detail<Activity>, String> {
    invoke_as("get_activity").await
}

pub async fn refresh_now() {
    let _ = invoke("refresh_now").await;
}

pub async fn test_notification() -> Result<(), String> {
    invoke("test_notification")
        .await
        .map(|_| ())
        .map_err(|e| e.as_string().unwrap_or_else(|| "Échec de l'envoi".into()))
}

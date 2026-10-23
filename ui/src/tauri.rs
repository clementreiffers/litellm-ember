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
    // Les champs Serde aplatis doivent rester des objets JSON, pas des Map JavaScript.
    let args = SaveArgs { input }
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|e| e.to_string())?;
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

pub async fn get_day_usage(
    date: String,
    force: bool,
) -> Result<Detail<shared::DailyUsage>, String> {
    get_day_usage_in_refresh(date, force, None).await
}

pub async fn begin_day_refresh(date: String) -> Result<Detail<u64>, String> {
    #[derive(Serialize)]
    struct Args {
        date: String,
    }
    let args = serde_wasm_bindgen::to_value(&Args { date }).map_err(|e| e.to_string())?;
    let value = invoke_with("begin_day_refresh", args).await.map_err(|e| {
        e.as_string()
            .unwrap_or_else(|| "Actualisation indisponible".into())
    })?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn get_day_usage_in_refresh(
    date: String,
    force: bool,
    refresh: Option<u64>,
) -> Result<Detail<shared::DailyUsage>, String> {
    let bounds = crate::usage::local_day_bounds(&date);
    #[derive(Serialize)]
    struct Args {
        date: String,
        force: bool,
        refresh: Option<u64>,
    }
    let args = serde_wasm_bindgen::to_value(&Args {
        date: date.clone(),
        force,
        refresh,
    })
    .map_err(|e| e.to_string())?;
    let value = invoke_with("get_day_usage", args).await.map_err(|e| {
        e.as_string()
            .unwrap_or_else(|| "Consommation indisponible".into())
    })?;
    if bounds != crate::usage::local_day_bounds(&date) {
        return Err("Le fuseau horaire a changé ; rechargez cette journée".into());
    }
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn get_cached_day(date: String) -> Result<Detail<Option<shared::CachedDaily>>, String> {
    let bounds = crate::usage::local_day_bounds(&date);
    #[derive(Serialize)]
    struct Args {
        date: String,
    }
    let args =
        serde_wasm_bindgen::to_value(&Args { date: date.clone() }).map_err(|e| e.to_string())?;
    let value = invoke_with("get_cached_day", args).await.map_err(|e| {
        e.as_string()
            .unwrap_or_else(|| "Cache quotidien indisponible".into())
    })?;
    if bounds != crate::usage::local_day_bounds(&date) {
        return Err("Le fuseau horaire a changé ; rechargez cette journée".into());
    }
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn get_day_costs(
    date: String,
    force: bool,
) -> Result<Detail<shared::DailyCosts>, String> {
    get_day_costs_in_refresh(date, force, None).await
}

pub async fn get_day_costs_in_refresh(
    date: String,
    force: bool,
    refresh: Option<u64>,
) -> Result<Detail<shared::DailyCosts>, String> {
    let bounds = crate::usage::local_day_bounds(&date);
    #[derive(Serialize)]
    struct Args {
        date: String,
        force: bool,
        refresh: Option<u64>,
    }
    let args = serde_wasm_bindgen::to_value(&Args {
        date: date.clone(),
        force,
        refresh,
    })
    .map_err(|e| e.to_string())?;
    let value = invoke_with("get_day_costs", args).await.map_err(|e| {
        e.as_string()
            .unwrap_or_else(|| "Coûts indisponibles".into())
    })?;
    if bounds != crate::usage::local_day_bounds(&date) {
        return Err("Le fuseau horaire a changé ; rechargez cette journée".into());
    }
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
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

pub async fn get_update_status() -> Result<shared::UpdateStatus, String> {
    invoke_as("get_update_status").await
}

pub async fn check_for_updates() -> Result<shared::UpdateStatus, String> {
    invoke_as("check_for_updates").await
}

pub async fn open_update_release(expected_version: String) -> Result<(), String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Args {
        expected_version: String,
    }
    let args =
        serde_wasm_bindgen::to_value(&Args { expected_version }).map_err(|e| e.to_string())?;
    invoke_with("open_update_release", args)
        .await
        .map(|_| ())
        .map_err(|e| {
            e.as_string()
                .unwrap_or_else(|| "Ouverture de la release impossible".into())
        })
}

pub async fn install_update(expected_version: String) -> Result<shared::UpdateStatus, String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Args {
        expected_version: String,
    }
    let args =
        serde_wasm_bindgen::to_value(&Args { expected_version }).map_err(|e| e.to_string())?;
    let value = invoke_with("install_update", args).await.map_err(|e| {
        e.as_string()
            .unwrap_or_else(|| "Installation impossible".into())
    })?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn restart_after_update() -> Result<(), String> {
    invoke_as("restart_after_update").await
}

pub async fn on_event<T: serde::de::DeserializeOwned + 'static>(
    name: &str,
    on_update: impl Fn(T) + 'static,
) -> Result<Listener, String> {
    let handler = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
        let payload =
            js_sys::Reflect::get(&event, &JsValue::from_str("payload")).unwrap_or(JsValue::NULL);
        if let Ok(value) = serde_wasm_bindgen::from_value(payload) {
            on_update(value);
        }
    });
    let unlisten = listen(name, &handler)
        .await
        .map_err(|_| "Abonnement aux mises à jour impossible")?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| "Réponse d'abonnement invalide")?;
    Ok(Listener {
        unlisten,
        _handler: handler,
    })
}

/// Lecture locale, actualisation légère ou synchronisation manuelle complète.
pub async fn get_local_history(
    mode: shared::HistoryMode,
) -> Result<shared::LocalHistorySnapshot, String> {
    #[derive(Serialize)]
    struct Args {
        mode: shared::HistoryMode,
    }
    let args = serde_wasm_bindgen::to_value(&Args { mode }).map_err(|e| e.to_string())?;
    let value = invoke_with("get_local_history", args).await.map_err(|e| {
        e.as_string()
            .unwrap_or_else(|| "Historique local indisponible".into())
    })?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn get_harness_snapshot() -> Result<shared::HarnessSnapshot, String> {
    invoke_as("get_harness_snapshot").await
}
pub async fn get_harness_settings() -> Result<shared::HarnessSettings, String> {
    invoke_as("get_harness_settings").await
}
pub async fn save_harness_settings(
    input: &shared::HarnessSettings,
) -> Result<shared::HarnessSettings, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        input: &'a shared::HarnessSettings,
    }
    let args = Args { input }
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|e| e.to_string())?;
    let value = invoke_with("save_harness_settings", args)
        .await
        .map_err(|e| {
            e.as_string()
                .unwrap_or_else(|| "Sauvegarde impossible".into())
        })?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}
pub async fn save_opencode_password(value: Option<String>) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args {
        value: Option<String>,
    }
    let args = Args { value }
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|e| e.to_string())?;
    invoke_with("save_opencode_password", args)
        .await
        .map_err(|e| {
            e.as_string()
                .unwrap_or_else(|| "Trousseau indisponible".into())
        })?;
    Ok(())
}

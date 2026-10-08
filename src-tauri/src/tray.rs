use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use shared::{Activity, SettingsInput, SettingsView, Stats, WeekDetails};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    ActivationPolicy, AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_positioner::{Position, WindowExt};
use tokio::sync::Notify;

use crate::{litellm::Client, settings::SettingsStore};

/// Délai d'ouverture pendant lequel la perte de focus n'est pas prise en compte immédiatement.
const GRACE: Duration = Duration::from_millis(600);

type SharedStats = Arc<Mutex<Stats>>;

#[tauri::command]
fn close_window(window: tauri::WebviewWindow) {
    let _ = window.hide();
}

#[tauri::command]
fn get_settings(store: State<'_, Arc<SettingsStore>>) -> SettingsView {
    store.view()
}

/// Enregistre les paramètres. Si l'endpoint ou la clé changent, les données affichées sont réinitialisées
/// et un rafraîchissement immédiat est demandé.
#[tauri::command]
fn save_settings(
    input: SettingsInput,
    store: State<'_, Arc<SettingsStore>>,
    stats: State<'_, SharedStats>,
    refresh: State<'_, Arc<Notify>>,
    app: AppHandle,
) -> Result<SettingsView, String> {
    let changed = store.save(input)?;
    if changed {
        let fresh = Stats::default();
        *stats.lock().unwrap() = fresh.clone();
        let _ = app.emit("stats-updated", &fresh);
    }
    refresh.notify_one();
    Ok(store.view())
}

fn client_for(store: &SettingsStore) -> Result<Client, String> {
    let key = store.api_key().ok_or("Aucune clé API : renseignez-la dans les paramètres.")?;
    Client::new(&store.get().base_url, key)
}

/// Onglet « Semaine », chargé à la demande.
#[tauri::command]
async fn get_week(
    store: State<'_, Arc<SettingsStore>>,
    stats: State<'_, SharedStats>,
) -> Result<WeekDetails, String> {
    let client = client_for(&store)?;
    let (start, reset, spend) = {
        let s = stats.lock().unwrap();
        (s.period_start.clone(), s.budget_reset_at.clone(), s.period_spend)
    };
    client.fetch_week(start.as_deref(), reset.as_deref(), spend).await
}

/// Onglet « Activité », chargé à la demande.
#[tauri::command]
async fn get_activity(store: State<'_, Arc<SettingsStore>>) -> Result<Activity, String> {
    client_for(&store)?.fetch_activity().await
}

/// Bouton « rafraîchir » du panneau : relance un cycle immédiatement.
#[tauri::command]
fn refresh_now(refresh: State<'_, Arc<Notify>>) {
    refresh.notify_one();
}

#[tauri::command]
fn get_stats(state: State<'_, SharedStats>) -> Stats {
    state.lock().unwrap().clone()
}

fn title_for(stats: &Stats) -> String {
    match (&stats.error, stats.updated_at.is_some()) {
        (Some(_), false) => "⚠︎ LiteLLM".to_string(),
        (Some(_), true) => format!("${:.2} ⚠︎", stats.total_spend),
        _ => format!("${:.2}", stats.total_spend),
    }
}

/// Un cycle : le prix d'abord, puis le détail par modèle et les tokens du jour en parallèle.
/// Chaque étape met à jour l'état dès qu'elle finit ; en cas d'erreur on garde les dernières valeurs.
async fn run_cycle(c: &Client, update: &(dyn Fn(&dyn Fn(&mut Stats)) + Sync)) {
    let key = match c.fetch_key().await {
        Ok(k) => k,
        Err(e) => return update(&|s| s.error = Some(e.clone())),
    };
    update(&|s| {
        s.total_spend = key.period_spend;
        s.period_spend = key.period_spend;
        s.max_budget = key.max_budget;
        s.budget_reset_at = key.budget_reset_at.clone();
        s.period_start = key.period_start.clone();
        s.error = None;
    });

    let models = async {
        match c.fetch_models(key.period_start.as_deref()).await {
            Ok(m) => update(&|s| s.models_total = m.clone()),
            Err(e) => update(&|s| s.error = Some(e.clone())),
        }
    };
    let today = async {
        match c.fetch_today().await {
            Ok(t) => update(&|s| {
                s.today = t.clone();
                s.today_date = Some(Local::now().format("%Y-%m-%d").to_string());
                s.updated_at = Some(Local::now().format("%H:%M:%S").to_string());
            }),
            Err(e) => update(&|s| s.error = Some(e.clone())),
        }
    };
    tokio::join!(models, today);
}

fn cache_path(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("stats.json"))
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("settings.json"))
}

fn load_cache(path: &Path) -> Option<Stats> {
    let mut stats: Stats = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    // Les tokens d'un autre jour ne doivent pas apparaître comme ceux d'aujourd'hui.
    if stats.today_date.as_deref() != Some(Local::now().format("%Y-%m-%d").to_string().as_str()) {
        stats.today.clear();
    }
    stats.error = None;
    Some(stats)
}

fn save_cache(path: &Path, stats: &Stats) {
    if let Ok(json) = serde_json::to_vec(stats) {
        let _ = std::fs::write(path, json);
    }
}

pub fn run() {
    let refresh = Arc::new(Notify::new());
    let stats: SharedStats = Arc::new(Mutex::new(Stats::default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .manage(stats.clone())
        .manage(refresh.clone())
        .invoke_handler(tauri::generate_handler![get_stats, close_window, get_settings, save_settings, get_week, get_activity, refresh_now])
        .setup(move |app| {
            app.set_activation_policy(ActivationPolicy::Accessory);

            let refresh_item = MenuItem::with_id(app, "refresh", "Rafraîchir", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&refresh_item, &quit_item])?;

            let notify = refresh.clone();
            let tray = TrayIconBuilder::with_id("main")
                .title("…")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "refresh" => notify.notify_one(),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // Cache disque : le dernier état connu s'affiche tout de suite, avant tout appel réseau.
            let cache = cache_path(app.handle());
            let store = Arc::new(SettingsStore::load(settings_path(app.handle())));
            app.manage(store.clone());
            if let Some(cached) = cache.as_deref().and_then(load_cache) {
                let _ = tray.set_title(Some(title_for(&cached)));
                *stats.lock().unwrap() = cached;
            }

            create_window(app.handle())?;

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let update = |f: &dyn Fn(&mut Stats)| {
                    let snapshot = {
                        let mut s = stats.lock().unwrap();
                        f(&mut s);
                        s.clone()
                    };
                    let _ = tray.set_title(Some(title_for(&snapshot)));
                    let _ = handle.emit("stats-updated", &snapshot);
                    if let Some(path) = &cache {
                        save_cache(path, &snapshot);
                    }
                };
                loop {
                    let settings = store.get();
                    match store.api_key() {
                        None => update(&|s| {
                            s.error = Some("Aucune clé API : renseignez-la dans les paramètres (⚙︎).".into())
                        }),
                        Some(key) => match Client::new(&settings.base_url, key) {
                            Ok(c) => run_cycle(&c, &update).await,
                            Err(e) => update(&|s| s.error = Some(e.clone())),
                        },
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(settings.refresh_secs.into())) => {}
                        _ = refresh.notified() => {}
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("erreur au lancement de l'application")
        .run(|_app, event| {
            // Garde-fou : l'app ne doit jamais quitter parce qu'une fenêtre se ferme.
            // Seul `app.exit(0)` (menu Quitter) porte un code de sortie et doit passer.
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}

/// Dernier instant d'affichage / de masquage par perte de focus (état partagé entre les handlers).
static SHOWN_AT: Mutex<Option<Instant>> = Mutex::new(None);
static BLUR_HIDDEN_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn since(slot: &Mutex<Option<Instant>>) -> Option<Duration> {
    slot.lock().ok().and_then(|g| g.map(|t| t.elapsed()))
}

const PANEL_RADIUS: f64 = 22.0;

/// Effet Liquid Glass (macOS 26+), avec repli sur le flou "HUD" sombre des versions précédentes.
#[cfg(target_os = "macos")]
fn apply_glass(window: &tauri::WebviewWindow) {
    use window_vibrancy::{
        apply_liquid_glass, apply_vibrancy, LiquidGlassOptions, NSGlassEffectViewStyle,
        NSVisualEffectMaterial, NSVisualEffectState,
    };
    let glass = LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular)
        .radius(PANEL_RADIUS)
        .tint_color((20, 20, 28, 90));
    if apply_liquid_glass(window, glass).is_err() {
        let _ = apply_vibrancy(
            window,
            NSVisualEffectMaterial::HudWindow,
            Some(NSVisualEffectState::Active),
            Some(PANEL_RADIUS),
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn apply_glass(_window: &tauri::WebviewWindow) {}

/// Crée le panneau une seule fois, caché et avec un fond sombre : l'afficher ensuite est instantané,
/// sans rechargement de la page ni flash blanc.
fn create_window(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("LiteLLM")
        .inner_size(460.0, 700.0)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        // Fenêtre transparente en thème sombre : le verre est posé derrière le webview.
        .transparent(true)
        // L'ombre native suit le rectangle de la fenêtre et dessine un cadre derrière les coins arrondis.
        .shadow(false)
        .theme(Some(tauri::Theme::Dark))
        .build()?;
    apply_glass(&window);

    // Masque le panneau au clic ailleurs. La perte de focus pendant l'ouverture est normale
    // (le clic sur le tray vole le focus un instant) : on revérifie à la fin du délai.
    let w = window.clone();
    window.on_window_event(move |e| {
        if let WindowEvent::Focused(false) = e {
            let w = w.clone();
            let wait = GRACE.saturating_sub(since(&SHOWN_AT).unwrap_or(GRACE));
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(wait).await;
                if w.is_visible().unwrap_or(false) && !w.is_focused().unwrap_or(true) {
                    *BLUR_HIDDEN_AT.lock().unwrap() = Some(Instant::now());
                    let _ = w.hide();
                }
            });
        }
    });
    Ok(())
}

fn toggle_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else { return };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    // Le clic sur le prix retire d'abord le focus au panneau, qui se masque : ce même clic
    // ne doit pas le rouvrir aussitôt.
    if since(&BLUR_HIDDEN_AT).is_some_and(|d| d < Duration::from_millis(300)) {
        return;
    }
    *SHOWN_AT.lock().unwrap() = Some(Instant::now());
    let _ = window.move_window(Position::TrayBottomCenter);
    let _ = window.show();
    let _ = window.set_focus();
    // Rafraîchit le contenu avec l'état courant (la page peut avoir été mise en veille).
    if let Ok(stats) = app.state::<SharedStats>().lock() {
        let _ = app.emit("stats-updated", &*stats);
    }
    // Si le focus n'a pas été pris du premier coup, on réessaie une fois.
    let w = window.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if w.is_visible().unwrap_or(false) && !w.is_focused().unwrap_or(true) {
            let _ = w.set_focus();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ember-tray-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn today() -> String {
        Local::now().format("%Y-%m-%d").to_string()
    }

    #[test]
    fn title_reflects_error_and_freshness() {
        let ok = Stats { total_spend: 4.256, ..Default::default() };
        assert_eq!(title_for(&ok), "$4.26");
        let err_no_data = Stats { error: Some("x".into()), ..Default::default() };
        assert_eq!(title_for(&err_no_data), "⚠︎ LiteLLM");
        let err_stale = Stats { error: Some("x".into()), updated_at: Some("10:00:00".into()), total_spend: 1.0, ..Default::default() };
        assert_eq!(title_for(&err_stale), "$1.00 ⚠︎");
    }

    #[test]
    fn cache_roundtrip_keeps_today_and_drops_the_error() {
        let path = tmp("fresh.json");
        let stats = Stats {
            today: vec![shared::ModelUsage { model: "m".into(), ..Default::default() }],
            today_date: Some(today()),
            error: Some("old error".into()),
            total_spend: 7.0,
            ..Default::default()
        };
        save_cache(&path, &stats);
        let loaded = load_cache(&path).unwrap();
        assert_eq!(loaded.today.len(), 1);
        assert_eq!(loaded.total_spend, 7.0);
        assert_eq!(loaded.error, None);
    }

    #[test]
    fn stale_cache_drops_todays_tokens_but_keeps_the_rest() {
        let path = tmp("stale.json");
        let stats = Stats {
            today: vec![shared::ModelUsage { model: "m".into(), ..Default::default() }],
            today_date: Some("2000-01-01".into()),
            total_spend: 7.0,
            ..Default::default()
        };
        save_cache(&path, &stats);
        let loaded = load_cache(&path).unwrap();
        assert!(loaded.today.is_empty());
        assert_eq!(loaded.total_spend, 7.0);

        // Sans date du tout, même traitement.
        save_cache(&path, &Stats { today: stats.today.clone(), ..Default::default() });
        assert!(load_cache(&path).unwrap().today.is_empty());
    }

    #[test]
    fn missing_or_corrupt_cache_yields_none() {
        assert!(load_cache(&tmp("absent.json")).is_none());
        let bad = tmp("bad.json");
        std::fs::write(&bad, b"nope").unwrap();
        assert!(load_cache(&bad).is_none());
    }

}

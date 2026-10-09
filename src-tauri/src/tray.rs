#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::persistence::load_cache;
#[cfg(test)]
use chrono::Local;
use shared::{Activity, Detail, SettingsInput, SettingsView, Stats, WeekDetails};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    ActivationPolicy, AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder,
    WindowEvent,
};
use tauri_plugin_positioner::{Position, WindowExt};

use crate::{notify, service::Service};

/// Délai d'ouverture pendant lequel la perte de focus n'est pas prise en compte immédiatement.
const GRACE: Duration = Duration::from_millis(600);

#[tauri::command]
fn close_window(window: tauri::WebviewWindow) {
    let _ = window.hide();
}

#[tauri::command]
async fn get_settings(service: State<'_, Service>) -> Result<SettingsView, String> {
    service.settings().await
}
#[tauri::command]
async fn save_settings(
    input: SettingsInput,
    service: State<'_, Service>,
) -> Result<SettingsView, String> {
    service.save(input).await
}
#[tauri::command]
async fn get_week(service: State<'_, Service>) -> Result<Detail<WeekDetails>, String> {
    service.week().await
}
#[tauri::command]
async fn get_activity(service: State<'_, Service>) -> Result<Detail<Activity>, String> {
    service.activity().await
}
#[tauri::command]
async fn refresh_now(service: State<'_, Service>) -> Result<(), String> {
    service.refresh_and_wait().await
}

/// Bouton « Tester » des paramètres.
#[tauri::command]
fn test_notification(app: AppHandle) -> Result<(), String> {
    notify::send_test(&app)
}

#[tauri::command]
fn get_stats(service: State<'_, Service>) -> Stats {
    service.stats()
}

fn title_for(stats: &Stats) -> String {
    match (&stats.error, stats.updated_at.is_some()) {
        (Some(_), false) => "⚠︎ LiteLLM".to_string(),
        (Some(_), true) => format!("${:.2} ⚠︎", stats.total_spend),
        _ => format!("${:.2}", stats.total_spend),
    }
}

fn data_file(app: &AppHandle, name: &str) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    Some(dir.join(name))
}

fn cache_path(app: &AppHandle) -> Option<PathBuf> {
    data_file(app, "stats.json")
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    data_file(app, "settings.json")
}

#[cfg(test)]
fn save_cache(path: &Path, stats: &Stats) {
    crate::persistence::write(path, stats).unwrap();
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            get_stats,
            close_window,
            get_settings,
            save_settings,
            get_week,
            get_activity,
            refresh_now,
            test_notification
        ])
        .setup(move |app| {
            app.set_activation_policy(ActivationPolicy::Accessory);

            let refresh_item = MenuItem::with_id(app, "refresh", "Rafraîchir", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&refresh_item, &quit_item])?;

            let tray = TrayIconBuilder::with_id("main")
                .title("…")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "refresh" => app.state::<Service>().refresh(),
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

            create_window(app.handle())?;
            let cache = cache_path(app.handle());
            let settings = settings_path(app.handle());
            let notifications = data_file(app.handle(), "notifications.json");
            let handle = app.handle().clone();
            let service = Service::start(
                settings,
                cache,
                notifications,
                handle.clone(),
                move |snapshot| {
                    let _ = tray.set_title(Some(title_for(snapshot)));
                    let _ = handle.emit("stats-updated", snapshot);
                },
            );
            app.manage(service);
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
        .title("Ember")
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
                    *BLUR_HIDDEN_AT.lock().expect("état de fenêtre empoisonné") =
                        Some(Instant::now());
                    let _ = w.hide();
                }
            });
        }
    });
    Ok(())
}

fn toggle_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    // Le clic sur le prix retire d'abord le focus au panneau, qui se masque : ce même clic
    // ne doit pas le rouvrir aussitôt.
    if since(&BLUR_HIDDEN_AT).is_some_and(|d| d < Duration::from_millis(300)) {
        return;
    }
    *SHOWN_AT.lock().expect("état de fenêtre empoisonné") = Some(Instant::now());
    let _ = window.move_window(Position::TrayBottomCenter);
    let _ = window.show();
    let _ = window.set_focus();
    // Rafraîchit le contenu avec l'état courant (la page peut avoir été mise en veille).
    let _ = app.emit("stats-updated", app.state::<Service>().stats());
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
        let ok = Stats {
            total_spend: 4.256,
            ..Default::default()
        };
        assert_eq!(title_for(&ok), "$4.26");
        let err_no_data = Stats {
            error: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(title_for(&err_no_data), "⚠︎ LiteLLM");
        let err_stale = Stats {
            error: Some("x".into()),
            updated_at: Some("10:00:00".into()),
            total_spend: 1.0,
            ..Default::default()
        };
        assert_eq!(title_for(&err_stale), "$1.00 ⚠︎");
    }

    #[test]
    fn cache_roundtrip_keeps_today_and_drops_the_error() {
        let path = tmp("fresh.json");
        let stats = Stats {
            today: vec![shared::ModelUsage {
                model: "m".into(),
                ..Default::default()
            }],
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
            today: vec![shared::ModelUsage {
                model: "m".into(),
                ..Default::default()
            }],
            today_date: Some("2000-01-01".into()),
            total_spend: 7.0,
            ..Default::default()
        };
        save_cache(&path, &stats);
        let loaded = load_cache(&path).unwrap();
        assert!(loaded.today.is_empty());
        assert_eq!(loaded.total_spend, 7.0);

        // Sans date du tout, même traitement.
        save_cache(
            &path,
            &Stats {
                today: stats.today.clone(),
                ..Default::default()
            },
        );
        assert!(load_cache(&path).unwrap().today.is_empty());
    }

    #[test]
    fn missing_or_corrupt_cache_yields_none() {
        assert!(load_cache(&tmp("absent.json")).is_none());
        let bad = tmp("bad.json");
        std::fs::write(&bad, b"nope").unwrap();
        assert!(load_cache(&bad).is_none());
    }

    mod cycle {
        use super::super::*;
        use crate::{
            litellm::Client,
            service::{apply, run_cycle, Event},
        };
        use httpmock::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Applique les mises à jour sur un `Stats` local, comme le fait le vrai `update`.
        async fn run(server: &MockServer, stats: &Mutex<Stats>, notified: &AtomicUsize) {
            let c = Client::new(&server.base_url(), "k".into()).unwrap();
            run_cycle(&c, |event| {
                let mut stats = stats.lock().unwrap();
                if matches!(event, Event::Key(_)) {
                    stats.error = None;
                    notified.fetch_add(1, Ordering::SeqCst);
                }
                if matches!(event, Event::Today(..)) {
                    stats.updated_at = Some(Local::now().format("%H:%M:%S").to_string());
                }
                apply(&mut stats, event);
            })
            .await;
        }

        fn mock_all(server: &MockServer) {
            server.mock(|when, then| {
                when.path("/key/info");
                then.status(200).json_body(serde_json::json!({
                    "info": {"spend": 10.0, "max_budget": 100.0, "budget_reset_at": "2026-10-12T00:00:00Z", "budget_duration": "7d"}
                }));
            });
            server.mock(|when, then| {
                when.path("/spend/logs").query_param("summarize", "true");
                then.status(200).json_body(serde_json::json!([{"startTime": "2026-10-05", "spend": 10.0, "models": {"m": 10.0}}]));
            });
            server.mock(|when, then| {
                when.path("/spend/logs").query_param("summarize", "false");
                then.status(200).json_body(serde_json::json!([]));
            });
        }

        #[tokio::test]
        async fn successful_cycle_fills_the_state_and_notifies_once() {
            let server = MockServer::start();
            mock_all(&server);
            let stats = Mutex::new(Stats {
                error: Some("avant".into()),
                ..Default::default()
            });
            let notified = AtomicUsize::new(0);
            run(&server, &stats, &notified).await;

            let s = stats.lock().unwrap();
            assert_eq!(s.period_spend, 10.0);
            assert_eq!(s.max_budget, Some(100.0));
            assert_eq!(s.period_start.as_deref(), Some("2026-10-05T00:00:00+00:00"));
            assert_eq!(s.models_total.len(), 1);
            assert_eq!(
                s.today_date,
                Some(Local::now().format("%Y-%m-%d").to_string())
            );
            assert!(s.updated_at.is_some());
            assert_eq!(s.error, None);
            assert_eq!(notified.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn key_failure_keeps_previous_values_and_skips_notification() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/key/info");
                then.status(500);
            });
            let stats = Mutex::new(Stats {
                total_spend: 3.0,
                updated_at: Some("09:00:00".into()),
                ..Default::default()
            });
            let notified = AtomicUsize::new(0);
            run(&server, &stats, &notified).await;

            let s = stats.lock().unwrap();
            assert_eq!(s.total_spend, 3.0);
            assert_eq!(s.updated_at.as_deref(), Some("09:00:00"));
            assert!(s.error.as_deref().unwrap().contains("500"));
            assert_eq!(notified.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn a_failing_detail_step_sets_the_error_but_keeps_the_price() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/key/info");
                then.status(200)
                    .json_body(serde_json::json!({"info": {"spend": 5.0}}));
            });
            server.mock(|when, then| {
                when.path("/spend/logs");
                then.status(503);
            });
            let stats = Mutex::new(Stats::default());
            let notified = AtomicUsize::new(0);
            run(&server, &stats, &notified).await;

            let s = stats.lock().unwrap();
            assert_eq!(s.total_spend, 5.0);
            assert!(s.error.is_some());
            assert_eq!(notified.load(Ordering::SeqCst), 1);
            assert_eq!(title_for(&s), "⚠︎ LiteLLM"); // pas encore de mise à jour complète
        }
    }
}

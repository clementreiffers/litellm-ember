use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use shared::Stats;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    ActivationPolicy, AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
    window::Color,
};
use tauri_plugin_positioner::{Position, WindowExt};
use tokio::sync::Notify;

use crate::litellm::Client;

/// Délai d'ouverture pendant lequel la perte de focus n'est pas prise en compte immédiatement.
const GRACE: Duration = Duration::from_millis(600);
const REFRESH_EVERY: Duration = Duration::from_secs(30);

type SharedStats = Arc<Mutex<Stats>>;

#[tauri::command]
fn close_window(window: tauri::WebviewWindow) {
    let _ = window.hide();
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
                s.updated_at = Some(Local::now().format("%H:%M:%S").to_string());
            }),
            Err(e) => update(&|s| s.error = Some(e.clone())),
        }
    };
    tokio::join!(models, today);
}

pub fn run() {
    let refresh = Arc::new(Notify::new());
    let stats: SharedStats = Arc::new(Mutex::new(Stats::default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .manage(stats.clone())
        .invoke_handler(tauri::generate_handler![get_stats, close_window])
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

            create_window(app.handle())?;

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let client = Client::from_env();
                // Applique une mise à jour partielle, puis met à jour le titre et le front.
                let update = |f: &dyn Fn(&mut Stats)| {
                    let snapshot = {
                        let mut s = stats.lock().unwrap();
                        f(&mut s);
                        s.clone()
                    };
                    let _ = tray.set_title(Some(title_for(&snapshot)));
                    let _ = handle.emit("stats-updated", &snapshot);
                };
                loop {
                    match &client {
                        Ok(c) => run_cycle(c, &update).await,
                        Err(e) => update(&|s| s.error = Some(e.clone())),
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(REFRESH_EVERY) => {}
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
        .background_color(Color(21, 21, 23, 255))
        .build()?;

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

    #[test]
    fn title_reflects_error_and_freshness() {
        let ok = Stats { total_spend: 4.256, ..Default::default() };
        assert_eq!(title_for(&ok), "$4.26");
        let err_no_data = Stats { error: Some("x".into()), ..Default::default() };
        assert_eq!(title_for(&err_no_data), "⚠︎ LiteLLM");
        let err_stale = Stats { error: Some("x".into()), updated_at: Some("10:00:00".into()), total_spend: 1.0, ..Default::default() };
        assert_eq!(title_for(&err_stale), "$1.00 ⚠︎");
    }
}

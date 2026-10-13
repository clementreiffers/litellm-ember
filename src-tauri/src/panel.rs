//! Placement du panneau en points AppKit, indépendant du Retina de l'ancien écran.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSEvent, NSScreen, NSWindow};
use objc2_foundation::{ns_string, NSNumber, NSPoint, NSRect, NSSize};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow, WindowEvent};

use crate::service::Service;

const WIDTH: f64 = 460.0;
const HEIGHT: f64 = 700.0;
const GRACE: Duration = Duration::from_millis(600);
const REOPEN_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
    #[error("{0}")]
    Unavailable(&'static str),
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl From<NSRect> for Rect {
    fn from(rect: NSRect) -> Self {
        Self {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        }
    }
}

impl Rect {
    fn contains_point(self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn contains(self, rect: Self) -> bool {
        // Tolérance pour l'arrondi aux pixels effectué par AppKit.
        const EPSILON: f64 = 1.0;
        rect.x >= self.x - EPSILON
            && rect.y >= self.y - EPSILON
            && rect.x + rect.width <= self.x + self.width + EPSILON
            && rect.y + rect.height <= self.y + self.height + EPSILON
    }
}

#[derive(Clone, Copy)]
struct Screen {
    id: u32,
    frame: Rect,
    visible: Rect,
    scale: f64,
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    screen: u32,
    x: f64,
}

/// AppKit place l'origine en bas à gauche, y croissant vers le haut.
fn placement(visible: Rect, center_x: f64) -> Rect {
    let width = WIDTH.min(visible.width);
    let height = HEIGHT.min(visible.height);
    Rect {
        x: (center_x - width / 2.0).clamp(visible.x, visible.x + visible.width - width),
        y: visible.y + visible.height - height,
        width,
        height,
    }
}

fn tray_center(rect: tauri::Rect, scale: f64) -> f64 {
    let position = rect.position.to_logical::<f64>(scale);
    let size = rect.size.to_logical::<f64>(scale);
    position.x + size.width / 2.0
}

fn choose_screen(screens: &[Screen], previous: Option<Anchor>, pointer: NSPoint) -> Option<Screen> {
    previous
        .and_then(|anchor| screens.iter().find(|screen| screen.id == anchor.screen))
        .or_else(|| {
            screens
                .iter()
                .find(|screen| screen.frame.contains_point(pointer.x, pointer.y))
        })
        // NSScreen.screens place l'écran principal en premier.
        .or_else(|| screens.first())
        .copied()
}

#[derive(Clone, Copy, Default)]
struct PanelState {
    generation: u64,
    shown_at: Option<Instant>,
    anchor: Option<Anchor>,
    blur_hidden: Option<(u32, Instant)>,
}

#[derive(Debug, PartialEq)]
enum Action {
    Show,
    Hide,
    Ignore,
}

impl PanelState {
    fn action(self, screen: u32, visible: bool, in_bounds: bool, now: Instant) -> Action {
        let same_screen = self.anchor.is_some_and(|anchor| anchor.screen == screen);
        if visible && same_screen && in_bounds {
            Action::Hide
        } else if !visible
            && in_bounds
            && self.blur_hidden.is_some_and(|(hidden_screen, at)| {
                hidden_screen == screen && now.saturating_duration_since(at) < REOPEN_DELAY
            })
        {
            Action::Ignore
        } else {
            Action::Show
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.shown_at = None;
        self.blur_hidden = None;
    }

    fn begin_show(&mut self, anchor: Anchor, now: Instant) -> u64 {
        self.invalidate();
        self.anchor = Some(anchor);
        self.shown_at = Some(now);
        self.generation
    }
}

static STATE: Mutex<PanelState> = Mutex::new(PanelState {
    generation: 0,
    shown_at: None,
    anchor: None,
    blur_hidden: None,
});

fn state<T>(f: impl FnOnce(&mut PanelState) -> T) -> T {
    // Un état empoisonné ne doit pas rendre le panneau définitivement inaccessible.
    f(&mut STATE.lock().unwrap_or_else(|error| error.into_inner()))
}

fn report(result: Result<()>) {
    if let Err(error) = result {
        eprintln!("Panneau Ember : {error}");
    }
}

fn on_main(
    window: &WebviewWindow,
    action: impl FnOnce(&WebviewWindow) -> Result<()> + Send + 'static,
) {
    let handle = window.clone();
    report(
        window
            .run_on_main_thread(move || report(action(&handle)))
            .map_err(Error::from),
    );
}

fn screens(mtm: MainThreadMarker) -> Result<Vec<Screen>> {
    NSScreen::screens(mtm)
        .iter()
        .map(|screen| {
            let id = screen
                .deviceDescription()
                .objectForKey(ns_string!("NSScreenNumber"))
                .and_then(|value| {
                    value
                        .downcast_ref::<NSNumber>()
                        .map(NSNumber::unsignedIntValue)
                })
                .ok_or(Error::Unavailable("Identifiant d'écran indisponible"))?;
            let visible = Rect::from(screen.visibleFrame());
            let scale = screen.backingScaleFactor();
            if visible.width <= 0.0 || visible.height <= 0.0 || !scale.is_finite() || scale <= 0.0 {
                return Err(Error::Unavailable("Géométrie d'écran indisponible"));
            }
            Ok(Screen {
                id,
                frame: screen.frame().into(),
                visible,
                scale,
            })
        })
        .collect()
}

fn with_native<T>(window: &WebviewWindow, f: impl FnOnce(&NSWindow) -> T) -> Result<T> {
    let _mtm = MainThreadMarker::new().ok_or(Error::Unavailable("Thread principal requis"))?;
    let pointer = window.ns_window()?.cast::<NSWindow>();
    // SAFETY: Tauri fournit le NSWindow du WebviewWindow encore vivant. L'accès est
    // limité au thread principal et à cet appel ; aucun pointeur natif n'est conservé.
    let native =
        unsafe { pointer.as_ref() }.ok_or(Error::Unavailable("Fenêtre native indisponible"))?;
    Ok(f(native))
}

fn hide(window: &WebviewWindow) -> Result<()> {
    state(PanelState::invalidate);
    window.hide()?;
    Ok(())
}

pub fn close(window: &WebviewWindow) {
    on_main(window, hide);
}

/// Les vérifications et leurs effets sont sérialisés sur le thread principal.
fn later(window: &WebviewWindow, generation: u64, delay: Duration, blur: bool) {
    let window = window.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        on_main(&window, move |window| {
            if !state(|state| state.generation == generation && state.shown_at.is_some())
                || !window.is_visible()?
                || window.is_focused()?
            {
                return Ok(());
            }
            if blur {
                window.hide()?;
                state(|state| {
                    let anchor = state.anchor;
                    state.invalidate();
                    state.blur_hidden = anchor.map(|anchor| (anchor.screen, Instant::now()));
                });
            } else {
                window.set_focus()?;
            }
            Ok(())
        });
    });
}

pub fn install(window: &WebviewWindow) {
    let handle = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event {
            let snapshot = state(|state| *state);
            if let Some(at) = snapshot.shown_at {
                later(
                    &handle,
                    snapshot.generation,
                    GRACE.saturating_sub(at.elapsed()),
                    true,
                );
            }
        }
    });
}

fn open(window: &WebviewWindow, click: Option<tauri::Rect>) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or(Error::Unavailable("Thread principal requis"))?;
    let screens = screens(mtm)?;
    let snapshot = state(|state| *state);
    let pointer = NSEvent::mouseLocation();
    let previous = if click.is_none() {
        snapshot.anchor
    } else {
        None
    };
    let screen = choose_screen(&screens, previous, pointer)
        .ok_or(Error::Unavailable("Aucun écran disponible"))?;
    let x = match click {
        Some(rect) => tray_center(rect, screen.scale),
        None => previous
            .filter(|anchor| anchor.screen == screen.id)
            .map_or(screen.visible.x + screen.visible.width / 2.0, |anchor| {
                anchor.x
            }),
    };
    let anchor = Anchor {
        screen: screen.id,
        x,
    };
    let frame = with_native(window, |native| Rect::from(native.frame()))?;
    if click.is_some() {
        match snapshot.action(
            screen.id,
            window.is_visible()?,
            screen.visible.contains(frame),
            Instant::now(),
        ) {
            Action::Hide => return hide(window),
            Action::Ignore => return Ok(()),
            Action::Show => {}
        }
    }
    let target = placement(screen.visible, x);
    let generation = state(|state| state.begin_show(anchor, Instant::now()));
    let result = (|| {
        with_native(window, |native| {
            native.setFrame_display(
                NSRect::new(
                    NSPoint::new(target.x, target.y),
                    NSSize::new(target.width, target.height),
                ),
                true,
            );
        })?;
        window.show()?;
        Ok(())
    })();
    if result.is_err() {
        state(PanelState::invalidate);
        return result;
    }
    // Un échec de focus n'annule pas une fenêtre correctement placée et affichée.
    report(window.set_focus().map_err(Error::from));
    later(window, generation, Duration::from_millis(150), false);
    // Vérifie aussi le focus si AppKit n'a pas émis de nouvelle perte de focus.
    later(window, generation, GRACE, true);
    let app = window.app_handle();
    if click.is_some() {
        app.emit("stats-updated", app.state::<Service>().stats())?;
    } else {
        app.emit("show-updates", ())?;
    }
    Ok(())
}

pub fn toggle_window(app: &AppHandle, rect: tauri::Rect) {
    if let Some(window) = app.get_webview_window("main") {
        // Les événements du tray sont reçus sur le thread principal : conserver
        // la position du pointeur correspondant au clic, sans tâche intermédiaire.
        report(open(&window, Some(rect)));
    }
}

pub fn show_updates(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        on_main(&window, |window| open(window, None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn screen(id: u32, frame: Rect, scale: f64) -> Screen {
        Screen {
            id,
            frame,
            visible: Rect {
                height: frame.height - 25.0,
                ..frame
            },
            scale,
        }
    }

    #[test]
    fn switching_retina_scales_does_not_depend_on_the_previous_screen() {
        let screens = [
            screen(1, rect(0.0, 0.0, 1920.0, 1080.0), 1.0),
            screen(2, rect(1920.0, 0.0, 1512.0, 982.0), 2.0),
        ];
        for index in [0, 1, 0, 1, 0] {
            let screen = screens[index];
            let logical_x = screen.frame.x + 700.0;
            let tray = tauri::Rect {
                position: tauri::PhysicalPosition::new(logical_x * screen.scale, 0.0).into(),
                size: tauri::PhysicalSize::new(80.0 * screen.scale, 25.0 * screen.scale).into(),
            };
            let target = placement(screen.visible, tray_center(tray, screen.scale));
            assert_eq!(target.x, logical_x + 40.0 - WIDTH / 2.0);
            assert_eq!((target.width, target.height), (WIDTH, HEIGHT));
            assert!(screen.visible.contains(target));
        }
    }

    #[test]
    fn placement_handles_identical_screens_and_negative_or_vertical_origins() {
        for (x, y) in [
            (0.0, 0.0),
            (1920.0, 0.0),
            (-1920.0, 0.0),
            (0.0, -1080.0),
            (0.0, 1080.0),
        ] {
            let area = rect(x, y, 1920.0, 1055.0);
            let target = placement(area, x + 900.0);
            assert_eq!(target.x, x + 670.0);
            assert_eq!(target.y, y + 355.0);
            assert!(area.contains(target));
        }
    }

    #[test]
    fn placement_clamps_both_edges_and_restores_nominal_size() {
        let area = rect(-1000.0, 50.0, 1000.0, 800.0);
        assert_eq!(placement(area, -1200.0).x, -1000.0);
        assert_eq!(placement(area, 200.0).x, -460.0);
        let small = rect(0.0, 50.0, 400.0, 600.0);
        assert_eq!(placement(small, 200.0), small);
        let restored = placement(area, -500.0);
        assert_eq!((restored.width, restored.height), (WIDTH, HEIGHT));
    }

    #[test]
    fn disconnected_screen_falls_back_to_pointer_then_primary() {
        let screens = [
            screen(1, rect(0.0, 0.0, 1920.0, 1080.0), 1.0),
            screen(2, rect(-1512.0, 0.0, 1512.0, 982.0), 2.0),
        ];
        let old = Some(Anchor { screen: 3, x: 0.0 });
        assert_eq!(
            choose_screen(&screens, old, NSPoint::new(-500.0, 500.0))
                .unwrap()
                .id,
            2
        );
        assert_eq!(
            choose_screen(&screens, old, NSPoint::new(9000.0, 500.0))
                .unwrap()
                .id,
            1
        );
        assert!(choose_screen(&[], old, NSPoint::new(0.0, 0.0)).is_none());
        let previous = Some(Anchor {
            screen: 2,
            x: -500.0,
        });
        assert_eq!(
            choose_screen(&screens, previous, NSPoint::new(500.0, 500.0))
                .unwrap()
                .id,
            2
        );
    }

    #[test]
    fn click_moves_between_screens_and_recovers_offscreen_windows() {
        let now = Instant::now();
        let mut state = PanelState::default();
        state.begin_show(
            Anchor {
                screen: 1,
                x: 500.0,
            },
            now,
        );
        assert_eq!(state.action(1, true, true, now), Action::Hide);
        assert_eq!(state.action(2, true, true, now), Action::Show);
        assert_eq!(state.action(1, true, false, now), Action::Show);
        state.blur_hidden = Some((1, now));
        assert_eq!(state.action(1, false, true, now), Action::Ignore);
        assert_eq!(state.action(2, false, true, now), Action::Show);
        assert_eq!(state.action(1, false, false, now), Action::Show);
        assert_eq!(
            state.action(1, false, true, now + REOPEN_DELAY),
            Action::Show
        );
    }

    #[test]
    fn reopening_or_failure_invalidates_old_focus_tasks_and_allows_retry() {
        let now = Instant::now();
        let mut state = PanelState::default();
        let first = state.begin_show(
            Anchor {
                screen: 1,
                x: 500.0,
            },
            now,
        );
        let second = state.begin_show(
            Anchor {
                screen: 2,
                x: 2400.0,
            },
            now,
        );
        assert_ne!(first, second);
        state.invalidate();
        assert_ne!(state.generation, second);
        assert!(state.shown_at.is_none());
        assert_eq!(state.action(2, false, true, now), Action::Show);
        let retry = state.begin_show(
            Anchor {
                screen: 2,
                x: 2400.0,
            },
            now,
        );
        assert_ne!(retry, second);
        assert_eq!(state.shown_at, Some(now));
    }
}

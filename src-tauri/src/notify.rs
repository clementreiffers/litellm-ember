//! Notifications macOS natives quand le budget de la période atteint un seuil.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Critical,
}

/// Décide quelle notification envoyer. Chaque seuil n'est signalé qu'une fois par période ;
/// si plusieurs seuils sont franchis d'un coup (premier lancement tardif), seul le plus haut est notifié.
/// Un seuil repasse « non signalé » si la consommation redescend sous lui.
pub fn decide(pct: f64, info: u32, critical: u32, fired: &mut BTreeSet<u32>) -> Option<(Level, u32)> {
    fired.retain(|t| f64::from(*t) <= pct);
    let crossed: Vec<(Level, u32)> = [(Level::Info, info), (Level::Critical, critical)]
        .into_iter()
        .filter(|(_, t)| pct >= f64::from(*t) && !fired.contains(t))
        .collect();
    fired.extend(crossed.iter().map(|(_, t)| *t));
    crossed.last().copied()
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Identifie la période de budget (date de reset) : un nouveau reset remet tout à zéro.
    period: Option<String>,
    fired: BTreeSet<u32>,
}

pub struct Notifier {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl Notifier {
    /// L'état est persisté pour ne pas renvoyer une notification déjà vue après un redémarrage.
    pub fn load(path: Option<PathBuf>) -> Self {
        let state = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, state: Mutex::new(state) }
    }

    /// Évalue les seuils et persiste leur état avant l’intégration du transport natif.
    fn check(&self, cfg: &Settings, spend: f64, max_budget: Option<f64>, reset_at: Option<&str>) -> Option<Notice> {
        let mut st = self.state.lock().unwrap();
        let notice = st.evaluate(cfg, spend, max_budget, reset_at);
        if let Some(path) = &self.path {
            if let Ok(json) = serde_json::to_vec(&*st) {
                let _ = std::fs::write(path, json);
            }
        }
        notice
    }
}

/// Notification à envoyer, avec le contexte nécessaire pour la rédiger.
#[derive(Debug, PartialEq)]
struct Notice {
    level: Level,
    threshold: u32,
    pct: f64,
    max: f64,
}

impl State {
    /// Partie pure de `Notifier::check` : met à jour l'état et renvoie la notification éventuelle.
    fn evaluate(&mut self, cfg: &Settings, spend: f64, max_budget: Option<f64>, reset_at: Option<&str>) -> Option<Notice> {
        if !cfg.notifications_enabled {
            return None;
        }
        let max = max_budget.filter(|m| *m > 0.0)?;
        let pct = spend / max * 100.0;

        let period = reset_at.map(str::to_string);
        if self.period != period {
            self.period = period;
            self.fired.clear();
        }
        decide(pct, cfg.notify_info_percent, cfg.notify_critical_percent, &mut self.fired)
            .map(|(level, threshold)| Notice { level, threshold, pct, max })
    }
}

fn message(level: Level, threshold: u32, pct: f64, spend: f64, max: f64, reset_at: Option<&str>) -> (String, String) {
    let remaining = (max - spend).max(0.0);
    let reset = reset_at.map(|d| format!(" jusqu'au reset du {}", d.get(..10).unwrap_or(d))).unwrap_or_default();
    let body = format!("${spend:.2} dépensés sur ${max:.0} · il reste ${remaining:.2}{reset}.");
    let title = match level {
        Level::Info => format!("💸 Budget LiteLLM : {threshold} % consommé"),
        Level::Critical => format!("⚠️ Budget LiteLLM : plus que {:.0} % restant", (100.0 - pct).max(0.0)),
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_each_threshold_once() {
        let mut fired = BTreeSet::new();
        assert_eq!(decide(30.0, 50, 75, &mut fired), None);
        assert_eq!(decide(52.0, 50, 75, &mut fired), Some((Level::Info, 50)));
        assert_eq!(decide(60.0, 50, 75, &mut fired), None); // déjà signalé
        assert_eq!(decide(76.0, 50, 75, &mut fired), Some((Level::Critical, 75)));
        assert_eq!(decide(99.0, 50, 75, &mut fired), None);
    }

    #[test]
    fn notifies_only_the_highest_threshold_when_both_are_crossed() {
        let mut fired = BTreeSet::new();
        assert_eq!(decide(80.0, 50, 75, &mut fired), Some((Level::Critical, 75)));
        assert_eq!(decide(81.0, 50, 75, &mut fired), None); // le seuil info n'arrive pas en retard
    }

    #[test]
    fn rearms_when_consumption_drops_below_a_threshold() {
        let mut fired = BTreeSet::new();
        assert!(decide(55.0, 50, 75, &mut fired).is_some());
        assert_eq!(decide(40.0, 50, 75, &mut fired), None);
        assert_eq!(decide(55.0, 50, 75, &mut fired), Some((Level::Info, 50)));
    }

    #[test]
    fn messages_use_the_right_emoji() {
        let (t, b) = message(Level::Info, 50, 51.0, 63.75, 125.0, Some("2026-10-12T00:00:00+00:00"));
        assert!(t.starts_with("💸") && t.contains("50 %"));
        assert!(b.contains("$63.75") && b.contains("$61.25") && b.contains("2026-10-12"));
        let (t, _) = message(Level::Critical, 75, 76.0, 95.0, 125.0, None);
        assert!(t.starts_with("⚠️") && t.contains("24 %"));
    }

    fn cfg(enabled: bool) -> Settings {
        Settings { notifications_enabled: enabled, notify_info_percent: 50, notify_critical_percent: 75, ..Settings::default() }
    }

    #[test]
    fn threshold_is_inclusive() {
        let mut fired = BTreeSet::new();
        assert_eq!(decide(50.0, 50, 75, &mut fired), Some((Level::Info, 50)));
    }

    #[test]
    fn nan_and_infinite_percentages_do_not_panic() {
        let mut fired = BTreeSet::new();
        assert_eq!(decide(f64::NAN, 50, 75, &mut fired), None);
        assert_eq!(decide(f64::INFINITY, 50, 75, &mut fired), Some((Level::Critical, 75)));
    }

    #[test]
    fn rearms_after_dropping_below_both_thresholds() {
        let mut fired = BTreeSet::new();
        assert_eq!(decide(80.0, 50, 75, &mut fired), Some((Level::Critical, 75)));
        assert_eq!(decide(10.0, 50, 75, &mut fired), None);
        assert_eq!(decide(80.0, 50, 75, &mut fired), Some((Level::Critical, 75)));
    }

    #[test]
    fn evaluate_ignores_disabled_notifications_and_missing_budget() {
        let mut st = State::default();
        assert_eq!(st.evaluate(&cfg(false), 99.0, Some(100.0), None), None);
        assert_eq!(st.evaluate(&cfg(true), 99.0, None, None), None);
        assert_eq!(st.evaluate(&cfg(true), 99.0, Some(0.0), None), None);
        assert!(st.fired.is_empty());
    }

    #[test]
    fn evaluate_computes_percentage_and_fires_once() {
        let mut st = State::default();
        let n = st.evaluate(&cfg(true), 60.0, Some(100.0), Some("r1")).unwrap();
        assert_eq!((n.level, n.threshold, n.max), (Level::Info, 50, 100.0));
        assert!((n.pct - 60.0).abs() < 1e-9);
        assert_eq!(st.evaluate(&cfg(true), 61.0, Some(100.0), Some("r1")), None);
    }

    #[test]
    fn evaluate_resets_when_the_period_changes() {
        let mut st = State::default();
        assert!(st.evaluate(&cfg(true), 60.0, Some(100.0), Some("r1")).is_some());
        assert!(st.evaluate(&cfg(true), 60.0, Some(100.0), Some("r2")).is_some());
        // Passer de Some à None est aussi un changement de période.
        assert!(st.evaluate(&cfg(true), 60.0, Some(100.0), None).is_some());
    }

    #[test]
    fn state_survives_a_serde_roundtrip_and_tolerates_garbage() {
        let mut st = State::default();
        st.evaluate(&cfg(true), 60.0, Some(100.0), Some("r1"));
        let back: State = serde_json::from_slice(&serde_json::to_vec(&st).unwrap()).unwrap();
        assert_eq!(back.period.as_deref(), Some("r1"));
        assert!(back.fired.contains(&50));
        assert!(serde_json::from_slice::<State>(b"not json").is_err());
    }

    #[test]
    fn load_falls_back_to_default_on_missing_or_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("ember-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = Notifier::load(Some(dir.join("missing.json")));
        assert!(missing.state.lock().unwrap().fired.is_empty());
        let bad = dir.join("bad.json");
        std::fs::write(&bad, b"{{{").unwrap();
        let corrupt = Notifier::load(Some(bad));
        assert!(corrupt.state.lock().unwrap().period.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

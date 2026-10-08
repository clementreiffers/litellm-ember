//! Paramètres persistants : endpoint et fréquence dans un fichier JSON, clé API dans le Trousseau macOS.
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use shared::{KeySource, SettingsInput, SettingsView};

const DEFAULT_REFRESH_SECS: u32 = 30;
pub const MIN_REFRESH_SECS: u32 = 5;
pub const MAX_REFRESH_SECS: u32 = 3600;

const KEYCHAIN_SERVICE: &str = "io.github.clementreiffers.ember";
const KEYCHAIN_ACCOUNT: &str = "litellm-api-key";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub base_url: String,
    pub refresh_secs: u32,
    pub notifications_enabled: bool,
    /// Moitié du budget consommée par défaut.
    pub notify_info_percent: u32,
    /// Il reste 1/4 du budget par défaut.
    pub notify_critical_percent: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            refresh_secs: DEFAULT_REFRESH_SECS,
            notifications_enabled: true,
            notify_info_percent: 50,
            notify_critical_percent: 75,
        }
    }
}

/// Stockage de la clé API, abstrait pour pouvoir tester sans toucher au vrai Trousseau.
pub trait KeyStorage: Send + Sync {
    fn get(&self) -> Option<String>;
    fn set(&self, key: &str) -> Result<(), String>;
    fn delete(&self);
}

/// Trousseau macOS.
struct Keychain;

impl Keychain {
    fn entry() -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(|e| format!("Trousseau indisponible: {e}"))
    }
}

impl KeyStorage for Keychain {
    fn get(&self) -> Option<String> {
        Self::entry().ok().and_then(|e| e.get_password().ok())
    }

    fn set(&self, key: &str) -> Result<(), String> {
        Self::entry()?.set_password(key).map_err(|e| format!("Écriture dans le Trousseau impossible: {e}"))
    }

    fn delete(&self) {
        if let Ok(e) = Self::entry() {
            let _ = e.delete_credential();
        }
    }
}

/// Paramètres courants + clé en mémoire (lue une seule fois dans le Trousseau).
pub struct SettingsStore {
    path: Option<PathBuf>,
    settings: Mutex<Settings>,
    key: Mutex<Option<(String, KeySource)>>,
    storage: Box<dyn KeyStorage>,
}

/// Clé enregistrée dans le stockage, s'il y en a une.
fn resolve_key(storage: &dyn KeyStorage) -> Option<(String, KeySource)> {
    storage.get().filter(|k| !k.is_empty()).map(|k| (k, KeySource::Keychain))
}

impl SettingsStore {
    pub fn load(path: Option<PathBuf>) -> Self {
        Self::load_with(path, Box::new(Keychain))
    }

    fn load_with(path: Option<PathBuf>, storage: Box<dyn KeyStorage>) -> Self {
        let settings = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_default();
        let key = Mutex::new(resolve_key(storage.as_ref()));
        Self { path, settings: Mutex::new(settings), key, storage }
    }

    pub fn get(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn api_key(&self) -> Option<String> {
        self.key.lock().unwrap().as_ref().map(|(k, _)| k.clone())
    }

    pub fn view(&self) -> SettingsView {
        let s = self.get();
        SettingsView {
            base_url: s.base_url,
            refresh_secs: s.refresh_secs,
            notifications_enabled: s.notifications_enabled,
            notify_info_percent: s.notify_info_percent,
            notify_critical_percent: s.notify_critical_percent,
            key_source: self.key.lock().unwrap().as_ref().map(|(_, src)| *src).unwrap_or(KeySource::Missing),
        }
    }

    /// Valide puis enregistre. Renvoie `true` si l'endpoint ou la clé ont changé (les données affichées sont alors périmées).
    pub fn save(&self, input: SettingsInput) -> Result<bool, String> {
        let base_url = input.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err("Renseignez l’endpoint LiteLLM dans les paramètres.".into());
        }
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err("L'endpoint doit commencer par http:// ou https://".into());
        }
        if !(MIN_REFRESH_SECS..=MAX_REFRESH_SECS).contains(&input.refresh_secs) {
            return Err(format!("La fréquence doit être entre {MIN_REFRESH_SECS} et {MAX_REFRESH_SECS} secondes"));
        }

        if !(1..=99).contains(&input.notify_info_percent) || !(2..=100).contains(&input.notify_critical_percent) {
            return Err("Les seuils de notification doivent être entre 1 et 100 % du budget consommé".into());
        }
        if input.notify_info_percent >= input.notify_critical_percent {
            return Err("Le seuil d'information doit être inférieur au seuil critique".into());
        }

        let mut changed = false;
        if input.clear_key {
            self.storage.delete();
            *self.key.lock().unwrap() = resolve_key(self.storage.as_ref());
            changed = true;
        } else if let Some(k) = input.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            self.storage.set(k)?;
            *self.key.lock().unwrap() = Some((k.to_string(), KeySource::Keychain));
            changed = true;
        }

        let new = Settings {
            base_url,
            refresh_secs: input.refresh_secs,
            notifications_enabled: input.notifications_enabled,
            notify_info_percent: input.notify_info_percent,
            notify_critical_percent: input.notify_critical_percent,
        };
        {
            let mut cur = self.settings.lock().unwrap();
            changed |= cur.base_url != new.base_url;
            *cur = new.clone();
        }
        if let Some(path) = &self.path {
            let json = serde_json::to_vec_pretty(&new).map_err(|e| e.to_string())?;
            std::fs::write(path, json).map_err(|e| format!("Écriture des paramètres impossible: {e}"))?;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SettingsStore {
        SettingsStore {
            path: None,
            settings: Mutex::new(Settings { base_url: "https://x/llm".into(), refresh_secs: 30, ..Settings::default() }),
            key: Mutex::new(None),
            storage: Box::new(MemoryKeys::default()),
        }
    }

    /// Faux Trousseau partageable : on garde un `Arc` pour inspecter ce qui a été écrit.
    #[derive(Clone, Default)]
    struct MemoryKeys {
        value: std::sync::Arc<Mutex<Option<String>>>,
        fail_set: bool,
    }

    impl KeyStorage for MemoryKeys {
        fn get(&self) -> Option<String> {
            self.value.lock().unwrap().clone()
        }
        fn set(&self, key: &str) -> Result<(), String> {
            if self.fail_set {
                return Err("refusé".into());
            }
            *self.value.lock().unwrap() = Some(key.to_string());
            Ok(())
        }
        fn delete(&self) {
            *self.value.lock().unwrap() = None;
        }
    }

    fn input(url: &str, secs: u32) -> SettingsInput {
        SettingsInput {
            base_url: url.into(),
            refresh_secs: secs,
            api_key: None,
            clear_key: false,
            notifications_enabled: true,
            notify_info_percent: 50,
            notify_critical_percent: 75,
        }
    }

    #[test]
    fn starts_without_a_default_endpoint() {
        assert!(Settings::default().base_url.is_empty());
    }

    #[test]
    fn empty_endpoints_leave_settings_untouched() {
        let s = store();
        for url in ["", "   "] {
            let error = s.save(input(url, 30)).unwrap_err();
            assert!(error.contains("endpoint LiteLLM"));
            assert_eq!(s.get().base_url, "https://x/llm");
            assert_eq!(s.get().refresh_secs, 30);
            assert_eq!(s.api_key(), None);
        }
    }

    #[test]
    fn rejects_invalid_values_without_touching_state() {
        let s = store();
        assert!(s.save(input("ftp://x", 30)).is_err());
        assert!(s.save(input("https://x", 1)).is_err());
        assert!(s.save(input("https://x", 99_999)).is_err());
        assert_eq!(s.get().base_url, "https://x/llm");
    }

    #[test]
    fn rejects_invalid_thresholds() {
        let s = store();
        let mut i = input("https://x", 30);
        i.notify_info_percent = 80;
        assert!(s.save(i.clone()).is_err()); // info >= critique
        i.notify_info_percent = 0;
        assert!(s.save(i.clone()).is_err());
        i.notify_info_percent = 50;
        i.notify_critical_percent = 101;
        assert!(s.save(i).is_err());
    }

    #[test]
    fn loads_old_settings_files_with_defaults() {
        let old: Settings = serde_json::from_str(r#"{"base_url":"https://x","refresh_secs":45}"#).unwrap();
        assert_eq!((old.notify_info_percent, old.notify_critical_percent), (50, 75));
        assert!(old.notifications_enabled);
    }

    #[test]
    fn saves_url_and_interval_and_reports_change() {
        let s = store();
        assert_eq!(s.save(input(" https://y/llm/ ", 60)), Ok(true));
        assert_eq!(s.get().base_url, "https://y/llm");
        assert_eq!(s.get().refresh_secs, 60);
        // Même endpoint, autre fréquence : les données restent valides.
        assert_eq!(s.save(input("https://y/llm", 15)), Ok(false));
    }

    #[test]
    fn load_reads_the_existing_key_and_reports_its_source() {
        let keys = MemoryKeys::default();
        *keys.value.lock().unwrap() = Some("sk-1".into());
        let st = SettingsStore::load_with(None, Box::new(keys));
        assert_eq!(st.api_key().as_deref(), Some("sk-1"));
        assert_eq!(st.view().key_source, KeySource::Keychain);

        let st = SettingsStore::load_with(None, Box::new(MemoryKeys::default()));
        assert_eq!(st.api_key(), None);
        assert_eq!(st.view().key_source, KeySource::Missing);
    }

    #[test]
    fn empty_stored_key_counts_as_missing() {
        let keys = MemoryKeys::default();
        *keys.value.lock().unwrap() = Some(String::new());
        assert_eq!(SettingsStore::load_with(None, Box::new(keys)).api_key(), None);
    }

    #[test]
    fn saving_a_key_stores_it_and_reports_a_change() {
        let keys = MemoryKeys::default();
        let st = SettingsStore { storage: Box::new(keys.clone()), ..store() };
        let mut i = input("https://x/llm", 30);
        i.api_key = Some("  sk-2  ".into());
        assert_eq!(st.save(i), Ok(true));
        assert_eq!(keys.get().as_deref(), Some("sk-2"));
        assert_eq!(st.api_key().as_deref(), Some("sk-2"));
        // Même endpoint, pas de nouvelle clé : rien n'a changé.
        assert_eq!(st.save(input("https://x/llm", 30)), Ok(false));
        assert_eq!(st.api_key().as_deref(), Some("sk-2"));
    }

    #[test]
    fn blank_key_input_keeps_the_current_key() {
        let keys = MemoryKeys::default();
        let st = SettingsStore { storage: Box::new(keys.clone()), ..store() };
        let mut i = input("https://x/llm", 30);
        i.api_key = Some("   ".into());
        assert_eq!(st.save(i), Ok(false));
        assert_eq!(keys.get(), None);
    }

    #[test]
    fn clearing_the_key_removes_it() {
        let keys = MemoryKeys::default();
        *keys.value.lock().unwrap() = Some("sk-3".into());
        let st = SettingsStore::load_with(None, Box::new(keys.clone()));
        let mut i = input("https://x/llm", 30);
        i.clear_key = true;
        assert_eq!(st.save(i), Ok(true));
        assert_eq!(keys.get(), None);
        assert_eq!(st.api_key(), None);
        assert_eq!(st.view().key_source, KeySource::Missing);
    }

    #[test]
    fn keychain_failure_leaves_settings_untouched() {
        let st = SettingsStore { storage: Box::new(MemoryKeys { fail_set: true, ..Default::default() }), ..store() };
        let mut i = input("https://other/llm", 60);
        i.api_key = Some("sk-4".into());
        assert!(st.save(i).is_err());
        assert_eq!(st.get().base_url, "https://x/llm");
        assert_eq!(st.get().refresh_secs, 30);
        assert_eq!(st.api_key(), None);
    }

    #[test]
    fn save_normalizes_the_url_and_changing_it_reports_a_change() {
        let st = store();
        assert_eq!(st.save(input("  https://y/llm//  ", 30)), Ok(true));
        assert_eq!(st.get().base_url, "https://y/llm");
    }

    #[test]
    fn threshold_boundaries() {
        let st = store();
        let with = |info, crit| SettingsInput { notify_info_percent: info, notify_critical_percent: crit, ..input("https://x/llm", 30) };
        assert!(st.save(with(1, 2)).is_ok());
        assert!(st.save(with(99, 100)).is_ok());
        assert!(st.save(with(0, 50)).is_err());
        assert!(st.save(with(50, 101)).is_err());
        assert!(st.save(with(60, 60)).is_err());
        assert!(st.save(input("https://x/llm", MIN_REFRESH_SECS)).is_ok());
        assert!(st.save(input("https://x/llm", MAX_REFRESH_SECS)).is_ok());
        assert!(st.save(input("https://x/llm", MIN_REFRESH_SECS - 1)).is_err());
        assert!(st.save(input("https://x/llm", MAX_REFRESH_SECS + 1)).is_err());
    }

    #[test]
    fn settings_roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("ember-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let st = SettingsStore { path: Some(path.clone()), ..store() };
        let mut i = input("https://z/llm", 120);
        i.notifications_enabled = false;
        i.notify_info_percent = 40;
        i.notify_critical_percent = 90;
        st.save(i).unwrap();

        let back = SettingsStore::load_with(Some(path.clone()), Box::new(MemoryKeys::default())).get();
        assert_eq!(back.base_url, "https://z/llm");
        assert_eq!(back.refresh_secs, 120);
        assert!(!back.notifications_enabled);
        assert_eq!((back.notify_info_percent, back.notify_critical_percent), (40, 90));

        // Un fichier corrompu retombe sur les valeurs par défaut.
        std::fs::write(&path, b"{{{").unwrap();
        let corrupt = SettingsStore::load_with(Some(path), Box::new(MemoryKeys::default())).get();
        assert_eq!(corrupt.refresh_secs, DEFAULT_REFRESH_SECS);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

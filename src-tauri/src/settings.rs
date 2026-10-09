//! Paramètres persistants : préférences JSON et clé dans le Trousseau macOS.
use crate::persistence::Prepared;
use serde::{Deserialize, Serialize};
use shared::{KeySource, SettingsInput, SettingsView};
use std::{path::PathBuf, sync::Mutex};

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
    pub notify_info_percent: u32,
    pub notify_critical_percent: u32,
    /// Identité opaque du cache ; ne contient aucune information sur la clé.
    pub source_id: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            refresh_secs: DEFAULT_REFRESH_SECS,
            notifications_enabled: true,
            notify_info_percent: 50,
            notify_critical_percent: 75,
            source_id: String::new(),
        }
    }
}

pub trait KeyStorage: Send + Sync {
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, key: &str) -> Result<(), String>;
    fn delete(&self) -> Result<(), String>;
}
struct Keychain;
impl Keychain {
    fn entry() -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .map_err(|e| format!("Trousseau indisponible: {e}"))
    }
}
impl KeyStorage for Keychain {
    fn get(&self) -> Result<Option<String>, String> {
        match Self::entry()?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("Lecture du Trousseau impossible: {e}")),
        }
    }
    fn set(&self, key: &str) -> Result<(), String> {
        Self::entry()?
            .set_password(key)
            .map_err(|e| format!("Écriture dans le Trousseau impossible: {e}"))
    }
    fn delete(&self) -> Result<(), String> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("Suppression dans le Trousseau impossible: {e}")),
        }
    }
}

pub fn validate_url(value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err("Renseignez l’endpoint LiteLLM dans les paramètres.".into());
    }
    let url = reqwest::Url::parse(value).map_err(|_| "Endpoint LiteLLM invalide")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("L'endpoint doit être une URL http:// ou https:// sans identifiants, paramètres ni fragment".into());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}
fn validate(input: &SettingsInput) -> Result<String, String> {
    let url = validate_url(&input.base_url)?;
    if !(MIN_REFRESH_SECS..=MAX_REFRESH_SECS).contains(&input.refresh_secs) {
        return Err(format!(
            "La fréquence doit être entre {MIN_REFRESH_SECS} et {MAX_REFRESH_SECS} secondes"
        ));
    }
    if !(1..=99).contains(&input.notify_info_percent)
        || !(2..=100).contains(&input.notify_critical_percent)
        || input.notify_info_percent >= input.notify_critical_percent
    {
        return Err("Les seuils doivent être entre 1 et 100 %, avec information < critique".into());
    }
    Ok(url)
}

pub struct SettingsStore {
    path: Option<PathBuf>,
    settings: Mutex<Settings>,
    key: Mutex<Option<(String, KeySource)>>,
    storage: Box<dyn KeyStorage>,
    operation: Mutex<()>,
    path_unavailable: bool,
    fault: Mutex<Option<String>>,
}
impl SettingsStore {
    pub fn load(path: Option<PathBuf>) -> Self {
        let unavailable = path.is_none();
        let directory_error = path
            .as_ref()
            .and_then(|p| p.parent())
            .and_then(|parent| std::fs::create_dir_all(parent).err());
        let mut store = Self::load_with(path, Box::new(Keychain));
        if let Some(error) = directory_error {
            *store.fault.lock().expect("état initial") =
                Some(format!("Répertoire des paramètres inaccessible: {error}"));
        }
        store.path_unavailable = unavailable;
        if unavailable {
            *store.fault.lock().expect("état initial") =
                Some("Répertoire des paramètres inaccessible".into());
        }
        store
    }
    fn load_with(path: Option<PathBuf>, storage: Box<dyn KeyStorage>) -> Self {
        let loaded = path
            .as_ref()
            .map(|p| match std::fs::read(p) {
                Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
                    .map_err(|_| "Fichier de paramètres invalide".to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
                Err(e) => Err(format!("Lecture des paramètres impossible: {e}")),
            })
            .unwrap_or_else(|| Ok(Settings::default()));
        let mut fault = loaded.as_ref().err().cloned();
        let mut settings = loaded.unwrap_or_default();
        if !settings.base_url.is_empty() {
            let input = SettingsInput {
                base_url: settings.base_url.clone(),
                refresh_secs: settings.refresh_secs,
                notifications_enabled: settings.notifications_enabled,
                notify_info_percent: settings.notify_info_percent,
                notify_critical_percent: settings.notify_critical_percent,
                ..Default::default()
            };
            if let Err(e) = validate(&input) {
                fault = Some(e);
            }
        }
        if fault.is_none() && !settings.base_url.is_empty() && settings.source_id.is_empty() {
            settings.source_id = uuid::Uuid::new_v4().to_string();
            if let Some(path) = &path {
                if let Err(e) = crate::persistence::write(path, &settings) {
                    fault = Some(e.to_string());
                }
            }
        }
        let key = match storage.get() {
            Ok(k) => k
                .filter(|k| !k.is_empty())
                .map(|k| (k, KeySource::Keychain)),
            Err(e) => {
                fault = Some(e);
                None
            }
        };
        Self {
            path,
            storage,
            settings: Mutex::new(settings),
            key: Mutex::new(key),
            operation: Mutex::new(()),
            path_unavailable: false,
            fault: Mutex::new(fault),
        }
    }
    #[cfg(test)]
    pub fn for_test(url: &str) -> Self {
        let store = tests::store();
        let mut input = tests::input(url, 30);
        input.api_key = Some("test-key".into());
        store.save(input).unwrap();
        store
    }
    pub fn get(&self) -> Settings {
        self.settings
            .lock()
            .expect("état des paramètres empoisonné")
            .clone()
    }
    pub fn fault(&self) -> Option<String> {
        self.fault.lock().expect("état d'erreur empoisonné").clone()
    }
    pub fn api_key(&self) -> Option<String> {
        self.key
            .lock()
            .expect("état de clé empoisonné")
            .as_ref()
            .map(|(k, _)| k.clone())
    }
    pub fn view(&self) -> SettingsView {
        let s = self.get();
        SettingsView {
            base_url: s.base_url,
            refresh_secs: s.refresh_secs,
            notifications_enabled: s.notifications_enabled,
            notify_info_percent: s.notify_info_percent,
            notify_critical_percent: s.notify_critical_percent,
            key_source: if self.key.lock().expect("état de clé empoisonné").is_some() {
                KeySource::Keychain
            } else {
                KeySource::Missing
            },
        }
    }
    /// Prépare le JSON avant de toucher au Trousseau ; publie seulement après commit.
    pub fn save(&self, input: SettingsInput) -> Result<bool, String> {
        if self.path_unavailable {
            return Err("Répertoire des paramètres inaccessible".into());
        }
        let _operation = self
            .operation
            .lock()
            .map_err(|_| "Sauvegarde indisponible")?;
        let base_url = validate(&input)?;
        let previous = self.get();
        let old_key = self.storage.get()?;
        let next_key = if input.clear_key {
            None
        } else {
            input
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_owned)
                .or_else(|| old_key.clone())
        };
        let changed =
            previous.base_url != base_url || old_key != next_key || previous.source_id.is_empty();
        let new = Settings {
            base_url,
            refresh_secs: input.refresh_secs,
            notifications_enabled: input.notifications_enabled,
            notify_info_percent: input.notify_info_percent,
            notify_critical_percent: input.notify_critical_percent,
            source_id: if changed {
                uuid::Uuid::new_v4().to_string()
            } else {
                previous.source_id
            },
        };
        let prepared = self
            .path
            .as_ref()
            .map(|p| Prepared::new(p, &new))
            .transpose()
            .map_err(|e| e.to_string())?;
        if old_key != next_key {
            self.write_key(next_key.as_deref())?;
        }
        if let Some(prepared) = prepared {
            if let Err(error) = prepared.commit() {
                if old_key != next_key && self.write_key(old_key.as_deref()).is_err() {
                    let message = "Sauvegarde incohérente : restauration du Trousseau impossible. Enregistrez à nouveau les paramètres.".to_string();
                    *self.fault.lock().map_err(|_| "État indisponible")? = Some(message.clone());
                    return Err(message);
                }
                return Err(error.to_string());
            }
        }
        *self.settings.lock().map_err(|_| "État indisponible")? = new;
        *self.key.lock().map_err(|_| "État indisponible")? =
            next_key.map(|k| (k, KeySource::Keychain));
        *self.fault.lock().map_err(|_| "État indisponible")? = None;
        Ok(changed)
    }
    fn write_key(&self, key: Option<&str>) -> Result<(), String> {
        match key {
            Some(k) => self.storage.set(k),
            None => self.storage.delete(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn store() -> SettingsStore {
        SettingsStore {
            path: None,
            settings: Mutex::new(Settings {
                base_url: "https://x/llm".into(),
                source_id: "test-source".into(),
                refresh_secs: 30,
                ..Settings::default()
            }),
            key: Mutex::new(None),
            storage: Box::new(MemoryKeys::default()),
            operation: Mutex::new(()),
            path_unavailable: false,
            fault: Mutex::new(None),
        }
    }

    /// Faux Trousseau partageable : on garde un `Arc` pour inspecter ce qui a été écrit.
    #[derive(Clone, Default)]
    struct MemoryKeys {
        value: std::sync::Arc<Mutex<Option<String>>>,
        fail_set: bool,
    }

    impl KeyStorage for MemoryKeys {
        fn get(&self) -> Result<Option<String>, String> {
            Ok(self.value.lock().unwrap().clone())
        }
        fn set(&self, key: &str) -> Result<(), String> {
            if self.fail_set {
                return Err("refusé".into());
            }
            *self.value.lock().unwrap() = Some(key.to_string());
            Ok(())
        }
        fn delete(&self) -> Result<(), String> {
            *self.value.lock().unwrap() = None;
            Ok(())
        }
    }

    pub(super) fn input(url: &str, secs: u32) -> SettingsInput {
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
        let old: Settings =
            serde_json::from_str(r#"{"base_url":"https://x","refresh_secs":45}"#).unwrap();
        assert_eq!(
            (old.notify_info_percent, old.notify_critical_percent),
            (50, 75)
        );
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
        assert_eq!(
            SettingsStore::load_with(None, Box::new(keys)).api_key(),
            None
        );
    }

    #[test]
    fn saving_a_key_stores_it_and_reports_a_change() {
        let keys = MemoryKeys::default();
        let st = SettingsStore {
            storage: Box::new(keys.clone()),
            ..store()
        };
        let mut i = input("https://x/llm", 30);
        i.api_key = Some("  sk-2  ".into());
        assert_eq!(st.save(i), Ok(true));
        assert_eq!(keys.get().unwrap().as_deref(), Some("sk-2"));
        assert_eq!(st.api_key().as_deref(), Some("sk-2"));
        // Même endpoint, pas de nouvelle clé : rien n'a changé.
        assert_eq!(st.save(input("https://x/llm", 30)), Ok(false));
        assert_eq!(st.api_key().as_deref(), Some("sk-2"));
    }

    #[test]
    fn blank_key_input_keeps_the_current_key() {
        let keys = MemoryKeys::default();
        let st = SettingsStore {
            storage: Box::new(keys.clone()),
            ..store()
        };
        let mut i = input("https://x/llm", 30);
        i.api_key = Some("   ".into());
        assert_eq!(st.save(i), Ok(false));
        assert_eq!(keys.get().unwrap(), None);
    }

    #[test]
    fn clearing_the_key_removes_it() {
        let keys = MemoryKeys::default();
        *keys.value.lock().unwrap() = Some("sk-3".into());
        let st = SettingsStore::load_with(None, Box::new(keys.clone()));
        let mut i = input("https://x/llm", 30);
        i.clear_key = true;
        assert_eq!(st.save(i), Ok(true));
        assert_eq!(keys.get().unwrap(), None);
        assert_eq!(st.api_key(), None);
        assert_eq!(st.view().key_source, KeySource::Missing);
    }

    #[test]
    fn keychain_failure_leaves_settings_untouched() {
        let st = SettingsStore {
            storage: Box::new(MemoryKeys {
                fail_set: true,
                ..Default::default()
            }),
            ..store()
        };
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
        let with = |info, crit| SettingsInput {
            notify_info_percent: info,
            notify_critical_percent: crit,
            ..input("https://x/llm", 30)
        };
        assert!(st.save(with(1, 2)).is_ok());
        assert!(st.save(with(99, 100)).is_ok());
        assert!(st.save(with(0, 50)).is_err());
        assert!(st.save(with(50, 101)).is_err());
        assert!(st.save(with(60, 60)).is_err());
        assert!(st.save(input("https://x/llm", MIN_REFRESH_SECS)).is_ok());
        assert!(st.save(input("https://x/llm", MAX_REFRESH_SECS)).is_ok());
        assert!(st
            .save(input("https://x/llm", MIN_REFRESH_SECS - 1))
            .is_err());
        assert!(st
            .save(input("https://x/llm", MAX_REFRESH_SECS + 1))
            .is_err());
    }

    #[test]
    fn settings_roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("ember-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let st = SettingsStore {
            path: Some(path.clone()),
            ..store()
        };
        let mut i = input("https://z/llm", 120);
        i.notifications_enabled = false;
        i.notify_info_percent = 40;
        i.notify_critical_percent = 90;
        st.save(i).unwrap();

        let back =
            SettingsStore::load_with(Some(path.clone()), Box::new(MemoryKeys::default())).get();
        assert_eq!(back.base_url, "https://z/llm");
        assert_eq!(back.refresh_secs, 120);
        assert!(!back.notifications_enabled);
        assert_eq!(
            (back.notify_info_percent, back.notify_critical_percent),
            (40, 90)
        );

        // Un fichier corrompu retombe sur les valeurs par défaut.
        std::fs::write(&path, b"{{{").unwrap();
        let corrupt = SettingsStore::load_with(Some(path), Box::new(MemoryKeys::default())).get();
        assert_eq!(corrupt.refresh_secs, DEFAULT_REFRESH_SECS);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn rejects_unsafe_or_incomplete_endpoints_without_changing_the_key() {
        let store = store();
        for url in [
            "https://",
            "https://user:password@host",
            "https://host?q=secret",
            "https://host/#part",
            "file:///tmp/x",
        ] {
            assert!(store.save(input(url, 30)).is_err());
            assert_eq!(store.api_key(), None);
        }
    }

    #[test]
    fn failing_file_preparation_never_touches_key_or_memory() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let store = SettingsStore {
            path: Some(dir.join("missing/settings.json")),
            ..store()
        };
        let mut input = input("https://other", 60);
        input.api_key = Some("new-key".into());
        assert!(store.save(input).is_err());
        assert_eq!(store.get().base_url, "https://x/llm");
        assert_eq!(store.api_key(), None);
        assert_eq!(store.storage.get().unwrap(), None);
    }

    #[derive(Default)]
    struct FailingKeys {
        value: Mutex<Option<String>>,
        read: bool,
        delete: bool,
        restore: bool,
    }
    impl KeyStorage for FailingKeys {
        fn get(&self) -> Result<Option<String>, String> {
            if self.read {
                Err("lecture refusée".into())
            } else {
                Ok(self.value.lock().unwrap().clone())
            }
        }
        fn set(&self, key: &str) -> Result<(), String> {
            if self.restore && key == "old" {
                return Err("restauration refusée".into());
            }
            *self.value.lock().unwrap() = Some(key.into());
            Ok(())
        }
        fn delete(&self) -> Result<(), String> {
            if self.delete {
                return Err("suppression refusée".into());
            }
            *self.value.lock().unwrap() = None;
            Ok(())
        }
    }
    #[test]
    fn keychain_read_and_delete_failures_are_not_missing_keys() {
        let store = SettingsStore {
            storage: Box::new(FailingKeys {
                read: true,
                ..Default::default()
            }),
            ..store()
        };
        assert!(store
            .save(input("https://x/llm", 30))
            .unwrap_err()
            .contains("lecture"));
        let store = SettingsStore {
            storage: Box::new(FailingKeys {
                delete: true,
                value: Mutex::new(Some("old".into())),
                ..Default::default()
            }),
            ..super::tests::store()
        };
        let mut input = input("https://other", 60);
        input.clear_key = true;
        assert!(store.save(input).unwrap_err().contains("suppression"));
        assert_eq!(store.get().base_url, "https://x/llm");
        assert_eq!(store.storage.get().unwrap().as_deref(), Some("old"));
    }
    #[test]
    fn failed_rename_rolls_back_key_or_records_a_blocking_fault() {
        for fail_restore in [false, true] {
            let directory = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(&directory).unwrap();
            // Une destination répertoire fait échouer rename après la préparation du fichier voisin.
            let store = SettingsStore {
                path: Some(directory.clone()),
                storage: Box::new(FailingKeys {
                    value: Mutex::new(Some("old".into())),
                    restore: fail_restore,
                    ..Default::default()
                }),
                ..store()
            };
            let mut input = input("https://other", 60);
            input.api_key = Some("new".into());
            assert!(store.save(input).is_err());
            assert_eq!(store.get().base_url, "https://x/llm");
            assert_eq!(
                store.storage.get().unwrap().as_deref(),
                Some(if fail_restore { "new" } else { "old" })
            );
            assert_eq!(store.fault().is_some(), fail_restore);
            std::fs::remove_dir(directory).unwrap();
        }
    }
    #[test]
    fn invalid_loaded_preferences_are_reported_instead_of_polled() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{"base_url":"https://host","refresh_secs":0}"#).unwrap();
        let store = SettingsStore::load_with(Some(path), Box::new(MemoryKeys::default()));
        assert!(store.fault().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn unavailable_settings_directory_cannot_report_a_successful_save() {
        let store = SettingsStore {
            path_unavailable: true,
            ..store()
        };
        let mut input = input("https://other", 60);
        input.api_key = Some("new".into());
        assert!(store.save(input).unwrap_err().contains("Répertoire"));
        assert_eq!(store.api_key(), None);
        assert_eq!(store.storage.get().unwrap(), None);
    }
}

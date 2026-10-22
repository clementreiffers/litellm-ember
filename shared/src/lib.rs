use serde::{Deserialize, Serialize};

/// État non secret de la mise à jour ; les chemins et artefacts restent dans le backend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateStatus {
    pub revision: u64,
    pub current_version: String,
    pub available_version: Option<String>,
    pub phase: UpdatePhase,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub failed_phase: Option<UpdatePhase>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Verifying,
    Installing,
    ReadyToRestart,
    Restarting,
    Failed,
}

impl UpdatePhase {
    pub fn busy(self) -> bool {
        matches!(
            self,
            Self::Checking
                | Self::Downloading
                | Self::Verifying
                | Self::Installing
                | Self::Restarting
        )
    }
}

/// Coût cumulé d'un modèle (toutes dates confondues).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    pub model: String,
    pub spend: f64,
}

/// Consommation d'un modèle sur la journée.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub spend: f64,
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Consommation à la demande, distincte des statistiques de la période courante.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// Jour local au format YYYY-MM-DD.
    pub date: String,
    pub models: Vec<ModelUsage>,
}

/// Aperçu des coûts d'une journée locale, sans compteurs de tokens inventés.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyCosts {
    pub date: String,
    pub models: Vec<ModelCost>,
}

/// État complet envoyé au front. `#[serde(default)]` : un cache écrit par une ancienne version reste lisible.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// Identité persistante de la connexion, sans secret.
    pub source_id: String,
    /// Génération en mémoire et ordre des publications, pour rejeter les réponses obsolètes.
    pub generation: u64,
    pub revision: u64,
    pub cycle_id: u64,
    pub refreshing: bool,
    /// Coût de la période de budget en cours, affiché dans la menu bar.
    pub total_spend: f64,
    /// Dépense de la période de budget en cours (`/key/info`, remise à zéro périodiquement).
    pub period_spend: f64,
    pub max_budget: Option<f64>,
    /// Instant ISO 8601 du prochain reset ; accepte aussi les anciennes dates YYYY-MM-DD.
    pub budget_reset_at: Option<String>,
    /// Début ISO 8601 de la période ; accepte aussi les anciennes dates YYYY-MM-DD.
    pub period_start: Option<String>,
    pub models_total: Vec<ModelCost>,
    pub today: Vec<ModelUsage>,
    /// Jour (local, YYYY-MM-DD) auquel correspond `today`, pour ignorer un cache périmé.
    pub today_date: Option<String>,
    /// Heure locale de la dernière mise à jour réussie (HH:MM:SS).
    pub updated_at: Option<String>,
    pub error: Option<String>,
}

/// D'où vient la clé API utilisée par le backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum KeySource {
    /// Enregistrée dans le Trousseau macOS via les paramètres.
    Keychain,
    #[default]
    Missing,
}

/// Préférences de détection hebdomadaire, indépendantes des alertes de budget.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpendIncreaseSettings {
    #[serde(rename = "spend_increase_enabled")]
    pub enabled: bool,
    #[serde(rename = "spend_increase_percent")]
    pub percent: u32,
    #[serde(rename = "spend_increase_min_usd")]
    pub min_usd: f64,
}

impl Default for SpendIncreaseSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            percent: 50,
            min_usd: 3.0,
        }
    }
}

impl SpendIncreaseSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=1_000).contains(&self.percent) {
            return Err("La hausse doit être comprise entre 1 et 1 000 %");
        }
        if !self.min_usd.is_finite() || !(0.01..=1_000_000.0).contains(&self.min_usd) {
            return Err("Le surcoût minimum doit être compris entre 0,01 et 1 000 000 $");
        }
        Ok(())
    }
}

/// Paramètres tels que vus par le front. La clé API n'y figure jamais.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    #[serde(default, flatten)]
    pub spend_increase: SpendIncreaseSettings,
    pub base_url: String,
    pub refresh_secs: u32,
    pub key_source: KeySource,
    pub notifications_enabled: bool,
    /// Seuil d'information, en % du budget consommé.
    pub notify_info_percent: u32,
    /// Seuil critique, en % du budget consommé.
    pub notify_critical_percent: u32,
}

/// Paramètres envoyés par le front à l'enregistrement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsInput {
    #[serde(default, flatten)]
    pub spend_increase: SpendIncreaseSettings,
    pub base_url: String,
    pub refresh_secs: u32,
    /// `None` ou vide : on conserve la clé actuelle.
    pub api_key: Option<String>,
    /// Supprime la clé du Trousseau.
    pub clear_key: bool,
    pub notifications_enabled: bool,
    pub notify_info_percent: u32,
    pub notify_critical_percent: u32,
}

/// Coût d'un jour (UTC, YYYY-MM-DD).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayCost {
    pub date: String,
    pub spend: f64,
}

/// Onglet « Semaine » : évolution jour par jour et projection de fin de période.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WeekDetails {
    pub days: Vec<DayCost>,
    /// Dépense estimée à la fin de la période de budget, au rythme actuel.
    pub projection: Option<f64>,
}

/// Cache de prompts fournisseur, agrégé sur les requêtes éligibles du jour.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheUsage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub reported_requests: u64,
    pub eligible_requests: u64,
}

impl PromptCacheUsage {
    /// Part des tokens réutilisés, uniquement sur les requêtes renseignées.
    pub fn rate(&self) -> Option<f64> {
        (self.reported_requests > 0 && self.input_tokens > 0)
            .then(|| self.cached_tokens as f64 / self.input_tokens as f64)
    }
}

/// Activité d'un modèle sur la journée.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelActivity {
    pub model: String,
    pub requests: u64,
    #[serde(default)]
    pub prompt_cache: PromptCacheUsage,
    pub errors: u64,
    pub spend: f64,
    pub avg_cost: f64,
    pub avg_tokens: f64,
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HourBucket {
    pub requests: u64,
    pub spend: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TopRequest {
    pub model: String,
    pub spend: f64,
    pub total_tokens: u64,
    /// Heure locale (HH:MM).
    pub time: String,
}

/// Onglet « Activité » : statistiques du jour calculées sur les logs détaillés.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub requests: u64,
    pub errors: u64,
    pub avg_cost: f64,
    pub avg_tokens: f64,
    /// Part des hits parmi les requêtes dont le statut de cache LiteLLM est renseigné.
    pub cache_rate: Option<f64>,
    #[serde(default)]
    pub prompt_cache: PromptCacheUsage,
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
    /// Délai médian avant le premier token (requêtes en streaming).
    pub ttft_p50_ms: Option<f64>,
    pub per_model: Vec<ModelActivity>,
    /// 24 entrées, une par heure locale.
    pub hourly: Vec<HourBucket>,
    pub top_request: Option<TopRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_activity_without_prompt_cache_still_loads() {
        let activity = Activity {
            per_model: vec![ModelActivity::default()],
            ..Default::default()
        };
        let mut value = serde_json::to_value(activity).unwrap();
        value.as_object_mut().unwrap().remove("prompt_cache");
        value["per_model"][0]
            .as_object_mut()
            .unwrap()
            .remove("prompt_cache");
        let restored: Activity = serde_json::from_value(value).unwrap();
        assert_eq!(restored.prompt_cache, PromptCacheUsage::default());
        assert_eq!(
            restored.per_model[0].prompt_cache,
            PromptCacheUsage::default()
        );
    }

    #[test]
    fn prompt_cache_distinguishes_missing_zero_and_zero_input() {
        let mut usage = PromptCacheUsage::default();
        assert_eq!(usage.rate(), None);
        usage.reported_requests = 1;
        assert_eq!(usage.rate(), None);
        usage.input_tokens = 100;
        assert_eq!(usage.rate(), Some(0.0));
        usage.cached_tokens = 75;
        assert_eq!(usage.rate(), Some(0.75));
    }

    #[test]
    fn update_status_roundtrips_and_marks_only_active_operations_busy() {
        let status = UpdateStatus {
            current_version: "1.0.0".into(),
            available_version: Some("1.1.0".into()),
            phase: UpdatePhase::Checking,
            revision: 7,
            ..Default::default()
        };
        let back: UpdateStatus =
            serde_json::from_slice(&serde_json::to_vec(&status).unwrap()).unwrap();
        assert_eq!(back, status);
        assert!(back.phase.busy());
        assert!(!UpdatePhase::Failed.busy());
        assert!(!UpdatePhase::Available.busy());
        assert!(!UpdatePhase::ReadyToRestart.busy());
        for phase in [
            UpdatePhase::Downloading,
            UpdatePhase::Verifying,
            UpdatePhase::Installing,
            UpdatePhase::Restarting,
        ] {
            assert!(phase.busy());
        }
        let old: UpdateStatus =
            serde_json::from_str(r#"{"current_version":"1.0.0","phase":"Available"}"#).unwrap();
        assert_eq!(old.downloaded_bytes, 0);
        assert_eq!(old.failed_phase, None);
    }

    #[test]
    fn stats_roundtrip() {
        let stats = Stats {
            total_spend: 12.5,
            max_budget: Some(125.0),
            today: vec![ModelUsage {
                model: "m".into(),
                requests: 2,
                ..Default::default()
            }],
            updated_at: Some("10:00:00".into()),
            ..Default::default()
        };
        let back: Stats = serde_json::from_str(&serde_json::to_string(&stats).unwrap()).unwrap();
        assert_eq!(back, stats);
    }

    #[test]
    fn stats_from_an_older_cache_file_still_loads() {
        let old = r#"{"total_spend":3.0,"models_total":[]}"#;
        let stats: Stats = serde_json::from_str(old).unwrap();
        assert_eq!(stats.total_spend, 3.0);
        assert_eq!(stats.max_budget, None);
        assert!(stats.today.is_empty());
        assert_eq!(
            serde_json::from_str::<Stats>("{}").unwrap(),
            Stats::default()
        );
    }

    #[test]
    fn key_source_serializes_as_a_stable_string() {
        // Contrat avec le front : ces valeurs ne doivent pas changer silencieusement.
        let json = serde_json::to_string(&[KeySource::Keychain, KeySource::Missing]).unwrap();
        assert_eq!(json, r#"["Keychain","Missing"]"#);
        let back: Vec<KeySource> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, vec![KeySource::Keychain, KeySource::Missing]);
    }
}

/// Résultat d'un onglet associé à sa configuration de connexion.
#[derive(Clone, Serialize, Deserialize)]
pub struct Detail<T> {
    pub generation: u64,
    pub data: T,
}

#[cfg(test)]
mod increase_settings_tests {
    use super::*;
    #[test]
    fn defaults_and_flat_serialization_remain_compatible() {
        let old: SettingsInput = serde_json::from_str(r#"{"base_url":"https://x","refresh_secs":30,"api_key":null,"clear_key":false,"notifications_enabled":false,"notify_info_percent":50,"notify_critical_percent":75}"#).unwrap();
        assert_eq!(old.spend_increase, SpendIncreaseSettings::default());
        let mut input = old;
        input.spend_increase = SpendIncreaseSettings {
            enabled: false,
            percent: 75,
            min_usd: 4.5,
        };
        let value = serde_json::to_value(&input).unwrap();
        assert_eq!(value["spend_increase_enabled"], false);
        assert_eq!(value["spend_increase_percent"], 75);
        assert_eq!(value["spend_increase_min_usd"], 4.5);
        assert_eq!(
            serde_json::from_value::<SettingsInput>(value).unwrap(),
            input
        );
    }

    #[test]
    fn rejects_out_of_range_and_non_finite_preferences() {
        for percent in [0, 1001] {
            assert!(SpendIncreaseSettings {
                percent,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for min_usd in [0.0, -1.0, 1_000_001.0, f64::NAN, f64::INFINITY] {
            assert!(SpendIncreaseSettings {
                min_usd,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for (percent, min_usd) in [(1, 0.01), (1000, 1_000_000.0)] {
            assert!(SpendIncreaseSettings {
                percent,
                min_usd,
                ..Default::default()
            }
            .validate()
            .is_ok());
        }
    }
}

/// Mode de lecture de l'historique ; Force recalcule toute la fenêtre.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryMode {
    Cached,
    Refresh,
    Force,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryDay {
    pub date: String,
    pub spend: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryDetails {
    pub days: Vec<HistoryDay>,
    /// Lundi = 0 ; calculé côté backend sans dépendance calendrier dans le Wasm.
    pub first_weekday: u8,
    pub updated_at: Option<String>,
    pub error: Option<String>,
}

/// Calendrier local publié progressivement ; la révision ordonne commandes et événements.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalHistorySnapshot {
    pub generation: u64,
    pub revision: u64,
    pub calendar_id: String,
    pub loading: bool,
    pub details: HistoryDetails,
}
mod harness;
pub use harness::*;

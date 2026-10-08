use serde::{Deserialize, Serialize};

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

/// État complet envoyé au front. `#[serde(default)]` : un cache écrit par une ancienne version reste lisible.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// Coût de la semaine (période de budget en cours), affiché dans la menu bar.
    pub total_spend: f64,
    /// Dépense de la période de budget en cours (`/key/info`, remise à zéro périodiquement).
    pub period_spend: f64,
    pub max_budget: Option<f64>,
    /// Date de la prochaine remise à zéro du budget (YYYY-MM-DD).
    pub budget_reset_at: Option<String>,
    /// Premier jour de la période de budget en cours (YYYY-MM-DD).
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

/// Paramètres tels que vus par le front. La clé API n'y figure jamais.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub base_url: String,
    pub refresh_secs: u32,
    pub key_source: KeySource,
}

/// Paramètres envoyés par le front à l'enregistrement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsInput {
    pub base_url: String,
    pub refresh_secs: u32,
    /// `None` ou vide : on conserve la clé actuelle.
    pub api_key: Option<String>,
    /// Supprime la clé du Trousseau.
    pub clear_key: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_roundtrip() {
        let stats = Stats {
            total_spend: 12.5,
            max_budget: Some(125.0),
            today: vec![ModelUsage { model: "m".into(), requests: 2, ..Default::default() }],
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
        assert_eq!(serde_json::from_str::<Stats>("{}").unwrap(), Stats::default());
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

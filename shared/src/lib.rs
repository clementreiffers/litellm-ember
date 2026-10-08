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

/// État complet envoyé au front.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// Heure locale de la dernière mise à jour réussie (HH:MM:SS).
    pub updated_at: Option<String>,
    pub error: Option<String>,
}

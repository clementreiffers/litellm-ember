//! Vue locale des harnais ; aucun secret ni historique persistant.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Codex,
    Claude,
    Opencode,
}
impl Harness {
    pub const ALL: [Self; 3] = [Self::Codex, Self::Claude, Self::Opencode];
    pub fn key(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Opencode => "OpenCode",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessConfig {
    pub disabled: bool,
    /// Répertoire de données du harnais ; vide = emplacement habituel.
    pub root: String,
    /// Compatibilité avec les hooks installés par les versions précédentes.
    pub hooks: bool,
    /// Serveur OpenCode local, facultatif si détectable par son processus.
    pub endpoint: String,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessSettings {
    pub codex: HarnessConfig,
    pub claude: HarnessConfig,
    pub opencode: HarnessConfig,
}
impl HarnessSettings {
    pub fn get(&self, harness: Harness) -> &HarnessConfig {
        match harness {
            Harness::Codex => &self.codex,
            Harness::Claude => &self.claude,
            Harness::Opencode => &self.opencode,
        }
    }
    pub fn get_mut(&mut self, harness: Harness) -> &mut HarnessConfig {
        match harness {
            Harness::Codex => &mut self.codex,
            Harness::Claude => &mut self.claude,
            Harness::Opencode => &mut self.opencode,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Running,
    Waiting,
    Completed,
    Interrupted,
    #[default]
    Unknown,
}
impl AgentState {
    pub fn active(self) -> bool {
        matches!(self, Self::Running | Self::Waiting)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "En cours",
            Self::Waiting => "Attend votre réponse",
            Self::Completed => "Terminé",
            Self::Interrupted => "Interrompu",
            Self::Unknown => "État inconnu",
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Entrée non mise en cache, normalisée par chaque connecteur.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEstimate {
    pub model: String,
    pub tokens: TokenUsage,
    pub usd: Option<f64>,
    pub partial: bool,
    pub source: Option<String>,
    /// USD par million de tokens : entrée, sortie, lecture et écriture du cache.
    pub rates: [Option<f64>; 4],
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillUse {
    pub name: String,
    pub explicit: bool,
}
/// Remplissage du contexte : taille de la dernière requête et fenêtre du modèle si elle est connue.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used: u64,
    pub window: Option<u64>,
    /// Taille du contexte à chaque réponse, de la plus ancienne à la plus récente.
    pub history: Vec<u64>,
}
impl ContextUsage {
    /// Fraction remplie dans [0, 1] ; `None` si la fenêtre est inconnue.
    pub fn ratio(&self) -> Option<f64> {
        self.window
            .filter(|w| *w > 0)
            .map(|w| (self.used as f64 / w as f64).clamp(0., 1.))
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub parent_id: Option<String>,
    pub harness: Harness,
    pub project: String,
    /// Nom de la session donné par le harnais, s'il en a un.
    #[serde(default)]
    pub title: Option<String>,
    pub prompt: String,
    /// Métadonnées déclarées par le harnais, jamais inférées depuis le prompt.
    #[serde(default)]
    pub agent_path: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub prompt_unavailable: bool,
    pub model: Option<String>,
    pub state: AgentState,
    pub started_at: i64,
    pub tool: Option<String>,
    pub skills: Vec<SkillUse>,
    pub estimates: Vec<ModelEstimate>,
    #[serde(default)]
    pub context: Option<ContextUsage>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessStatus {
    pub harness: Harness,
    pub installed: bool,
    pub available: bool,
    pub coverage: String,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HarnessSnapshot {
    pub revision: u64,
    pub agents: Vec<Agent>,
    pub statuses: Vec<HarnessStatus>,
}
impl HarnessSnapshot {
    pub fn active_count(&self) -> usize {
        self.agents.iter().filter(|a| a.state.active()).count()
    }
    pub fn accept(&mut self, next: Self) {
        if next.revision >= self.revision {
            *self = next;
        }
    }
}

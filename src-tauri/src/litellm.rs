use std::collections::HashMap;

use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use serde::Deserialize;
use shared::{ModelCost, ModelUsage};

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    /// Lit `OPENAI_API_KEY` (obligatoire) et `LITELLM_BASE_URL` (obligatoire) dans l'environnement.
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or("OPENAI_API_KEY absente ou vide")?;
        let base_url = std::env::var("LITELLM_BASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .ok_or("LITELLM_BASE_URL absente ou vide")?
            .trim()
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .map_err(|e| e.to_string())?,
            base_url,
            api_key,
        })
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| describe(e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status} sur {path}"));
        }
        resp.json().await.map_err(|e| format!("réponse invalide ({path}): {e}"))
    }

    /// Étape 1 (≈0,5 s) : dépense de la période de budget en cours (`/key/info`).
    pub async fn fetch_key(&self) -> Result<KeyData, String> {
        let key: KeyInfo = self.get("/key/info").await?;
        Ok(KeyData {
            period_spend: key.info.spend,
            max_budget: key.info.max_budget,
            budget_reset_at: key.info.budget_reset_at.as_ref().map(|d| d.chars().take(10).collect()),
            period_start: period_start(&key.info),
        })
    }

    /// Étape 2 (≈0,5 s) : coût par modèle depuis le début de la période (agrégat, sans tokens).
    pub async fn fetch_models(&self, period_start: Option<&str>) -> Result<Vec<ModelCost>, String> {
        let now = Utc::now().date_naive();
        let tomorrow = now + Duration::days(1);
        let start = period_start.map(str::to_string).unwrap_or_else(|| (now - Duration::days(7)).to_string());
        let summary: Vec<DaySummary> = self
            .get(&format!("/spend/logs?summarize=true&start_date={start}&end_date={tomorrow}"))
            .await?;
        Ok(aggregate_totals(&summary))
    }

    /// Étape 3 (lente, 5 à 10 s : l'API renvoie les messages) : tokens et $ du jour par modèle.
    pub async fn fetch_today(&self) -> Result<Vec<ModelUsage>, String> {
        let today = Local::now().date_naive();
        // On ne demande que le jour UTC où commence le jour local, pas la veille entière.
        let start = today
            .and_hms_opt(0, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).earliest())
            .map(|d| d.with_timezone(&Utc).date_naive())
            .unwrap_or_else(|| Utc::now().date_naive());
        let end = Utc::now().date_naive() + Duration::days(1);
        let logs: Vec<LogRow> = self
            .get(&format!("/spend/logs?summarize=false&start_date={start}&end_date={end}"))
            .await?;
        Ok(aggregate_today(&logs, today))
    }
}

pub struct KeyData {
    pub period_spend: f64,
    pub max_budget: Option<f64>,
    pub budget_reset_at: Option<String>,
    pub period_start: Option<String>,
}

/// Début de la période de budget : date de reset moins la durée du budget (`7d`, `24h`, `2w`, `1mo`).
fn period_start(info: &KeyInfoInner) -> Option<String> {
    let reset = NaiveDate::parse_from_str(info.budget_reset_at.as_deref()?.get(..10)?, "%Y-%m-%d").ok()?;
    let days = parse_days(info.budget_duration.as_deref().unwrap_or("7d"));
    Some((reset - Duration::days(days)).to_string())
}

fn parse_days(duration: &str) -> i64 {
    let split = duration.find(|c: char| !c.is_ascii_digit()).unwrap_or(duration.len());
    let n: i64 = duration[..split].parse().unwrap_or(7);
    match &duration[split..] {
        "h" | "s" | "m" => 1,
        "w" => n * 7,
        "mo" => n * 30,
        _ => n,
    }
    .max(1)
}

/// Message d'erreur avec la chaîne de causes, sans l'URL.
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut msg = format!("requête échouée: {e}");
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(&format!(" ← {s}"));
        src = s.source();
    }
    msg
}

#[derive(Deserialize)]
struct KeyInfo {
    info: KeyInfoInner,
}

#[derive(Deserialize)]
struct KeyInfoInner {
    spend: f64,
    max_budget: Option<f64>,
    budget_reset_at: Option<String>,
    budget_duration: Option<String>,
}

#[derive(Deserialize)]
struct DaySummary {
    #[serde(default)]
    models: HashMap<String, f64>,
}

#[derive(Deserialize)]
struct LogRow {
    #[serde(default)]
    model: String,
    #[serde(default)]
    spend: f64,
    #[serde(default, rename = "prompt_tokens")]
    prompt: u64,
    #[serde(default, rename = "completion_tokens")]
    completion: u64,
    #[serde(default, rename = "total_tokens")]
    total: u64,
    #[serde(rename = "startTime")]
    start_time: Option<DateTime<Utc>>,
}

/// `openai/gpt-6-sol` et `gpt-6-sol` sont le même modèle ; `bedrock/invoke/x` → `x`.
/// Renvoie `None` pour un nom vide.
pub fn normalize_model(name: &str) -> Option<String> {
    let short = name.rsplit('/').next().unwrap_or(name).trim();
    (!short.is_empty()).then(|| short.to_string())
}

fn aggregate_totals(days: &[DaySummary]) -> Vec<ModelCost> {
    let mut acc: HashMap<String, f64> = HashMap::new();
    for (name, spend) in days.iter().flat_map(|d| d.models.iter()) {
        if let Some(model) = normalize_model(name) {
            *acc.entry(model).or_default() += spend;
        }
    }
    let mut out: Vec<_> = acc.into_iter().map(|(model, spend)| ModelCost { model, spend }).collect();
    out.sort_by(|a, b| b.spend.total_cmp(&a.spend).then(a.model.cmp(&b.model)));
    out
}

fn aggregate_today(rows: &[LogRow], today: NaiveDate) -> Vec<ModelUsage> {
    let mut acc: HashMap<String, ModelUsage> = HashMap::new();
    for row in rows {
        let Some(start) = row.start_time else { continue };
        if start.with_timezone(&Local).date_naive() != today {
            continue;
        }
        let Some(model) = normalize_model(&row.model) else { continue };
        let u = acc.entry(model.clone()).or_insert_with(|| ModelUsage { model, ..Default::default() });
        u.spend += row.spend;
        u.requests += 1;
        u.prompt_tokens += row.prompt;
        u.completion_tokens += row.completion;
        u.total_tokens += row.total;
    }
    let mut out: Vec<_> = acc.into_values().collect();
    out.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens).then(a.model.cmp(&b.model)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_budget_durations() {
        assert_eq!(parse_days("7d"), 7);
        assert_eq!(parse_days("2w"), 14);
        assert_eq!(parse_days("1mo"), 30);
        assert_eq!(parse_days("24h"), 1);
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_model("openai/gpt-6-sol").as_deref(), Some("gpt-6-sol"));
        assert_eq!(normalize_model("bedrock/invoke/eu.anthropic.x").as_deref(), Some("eu.anthropic.x"));
        assert_eq!(normalize_model("gpt-6-sol").as_deref(), Some("gpt-6-sol"));
        assert_eq!(normalize_model(""), None);
    }

    #[test]
    fn sums_totals_across_days_and_aliases() {
        let days: Vec<DaySummary> = serde_json::from_str(
            r#"[{"startTime":"2026-10-06","spend":1.0,"models":{"openai/gpt-6-sol":1.0,"":0.0}},
                {"startTime":"2026-10-07","spend":2.5,"models":{"gpt-6-sol":2.5,"other":0.5}}]"#,
        )
        .unwrap();
        let totals = aggregate_totals(&days);
        assert_eq!(totals.len(), 2);
        assert_eq!(totals[0], ModelCost { model: "gpt-6-sol".into(), spend: 3.5 });
    }

    #[test]
    fn aggregates_today_only() {
        let now = Utc::now().to_rfc3339();
        let old = (Utc::now() - Duration::days(3)).to_rfc3339();
        let rows: Vec<LogRow> = serde_json::from_str(&format!(
            r#"[{{"model":"openai/m","spend":0.5,"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"startTime":"{now}"}},
                {{"model":"m","spend":0.25,"prompt_tokens":5,"completion_tokens":1,"total_tokens":6,"startTime":"{now}"}},
                {{"model":"m","spend":9.0,"prompt_tokens":1,"completion_tokens":1,"total_tokens":2,"startTime":"{old}"}}]"#
        ))
        .unwrap();
        let today = aggregate_today(&rows, Local::now().date_naive());
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].requests, 2);
        assert_eq!(today[0].total_tokens, 18);
        assert!((today[0].spend - 0.75).abs() < 1e-9);
    }

    #[test]
    fn parse_days_handles_units_and_garbage() {
        assert_eq!(parse_days("7d"), 7);
        assert_eq!(parse_days("2w"), 14);
        assert_eq!(parse_days("1mo"), 30);
        assert_eq!(parse_days("30"), 30);
        // Les durées inférieures à un jour comptent pour un jour.
        assert_eq!(parse_days("24h"), 1);
        assert_eq!(parse_days("90m"), 1);
        assert_eq!(parse_days("0d"), 1);
        assert_eq!(parse_days(""), 7);
        assert_eq!(parse_days("abc"), 7);
        assert_eq!(parse_days("é"), 7);
        assert_eq!(parse_days("3D"), 3); // unité inconnue : on garde le nombre
    }

    #[test]
    fn period_start_subtracts_the_budget_duration() {
        let info = |reset: Option<&str>, dur: Option<&str>| KeyInfoInner {
            spend: 0.0,
            max_budget: None,
            budget_reset_at: reset.map(str::to_string),
            budget_duration: dur.map(str::to_string),
        };
        assert_eq!(period_start(&info(Some("2026-10-12T00:00:00Z"), Some("7d"))).as_deref(), Some("2026-10-05"));
        assert_eq!(period_start(&info(Some("2026-10-12T00:00:00Z"), None)).as_deref(), Some("2026-10-05"));
        assert_eq!(period_start(&info(Some("2026-10-31T00:00:00Z"), Some("1mo"))).as_deref(), Some("2026-10-01"));
        assert_eq!(period_start(&info(None, Some("7d"))), None);
        assert_eq!(period_start(&info(Some("pas une date"), Some("7d"))), None);
        assert_eq!(period_start(&info(Some("2026"), Some("7d"))), None);
    }

    #[test]
    fn normalize_model_edge_cases() {
        assert_eq!(normalize_model("a/").as_deref(), None);
        assert_eq!(normalize_model("   ").as_deref(), None);
        assert_eq!(normalize_model("").as_deref(), None);
        assert_eq!(normalize_model(" bedrock/invoke/x ").as_deref(), Some("x"));
    }

    #[test]
    fn totals_ignore_empty_model_names() {
        let days: Vec<DaySummary> =
            serde_json::from_str(r#"[{"models":{"":1.0,"a/m":2.0},"spend":3.0,"startTime":"2026-10-12"}]"#).unwrap();
        let totals = aggregate_totals(&days);
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].model, "m");
    }

}

#[cfg(test)]
mod live {
    use super::*;

    /// Appel réel : `cargo test -p litellm-menubar live -- --ignored --nocapture` (nécessite OPENAI_API_KEY et LITELLM_BASE_URL).
    #[tokio::test]
    #[ignore]
    async fn fetch_stats_live() {
        let c = Client::from_env().unwrap();
        let t = std::time::Instant::now();
        let key = c.fetch_key().await.unwrap();
        println!("période ${:.2} depuis {:?} ({:?})", key.period_spend, key.period_start, t.elapsed());
        let models = c.fetch_models(key.period_start.as_deref()).await.unwrap();
        println!("modèles ({:?})", t.elapsed());
        for m in &models {
            println!("{:<55} ${:.2}", m.model, m.spend);
        }
        let today = c.fetch_today().await.unwrap();
        println!("--- aujourd'hui ({:?})", t.elapsed());
        for u in &today {
            println!("{:<55} {:>10} tok ${:.2} ({} req)", u.model, u.total_tokens, u.spend, u.requests);
        }
    }
}

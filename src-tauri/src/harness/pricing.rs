//! Tarifs publics vérifiés le 2026-10-13 dans le catalogue LiteLLM (USD/token).
//! https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json
use serde_json::Value;
use shared::{ModelEstimate, TokenUsage};
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub struct Prices {
    pub remote: BTreeMap<String, [Option<f64>; 4]>,
}
impl Prices {
    pub fn from_litellm(v: &Value) -> Self {
        let mut result = Self::default();
        let mut ambiguous = std::collections::BTreeSet::new();
        if let Some(rows) = v["data"].as_array() {
            for row in rows {
                let Some(name) = row["model_name"].as_str() else {
                    continue;
                };
                let info = &row["model_info"];
                let rate = fields(info);
                if rate[0].is_none() || rate[1].is_none() {
                    continue;
                }
                if result.remote.get(name).is_some_and(|old| old != &rate) {
                    ambiguous.insert(name.to_owned());
                }
                result.remote.insert(name.to_owned(), rate);
            }
        }
        for name in ambiguous {
            result.remote.remove(&name);
        }
        result
    }
    pub fn estimate(&self, model: &str, tokens: TokenUsage, active: bool) -> ModelEstimate {
        let public = public_rates(model);
        let (rates, source) = if let Some(r) = self.remote.get(model) {
            (*r, Some("LiteLLM · tarifs de l’instance".into()))
        } else if let Some(r) = public {
            (
                r,
                Some("Catalogue public LiteLLM · tarif standard · 13/10/2026".into()),
            )
        } else {
            ([None; 4], None)
        };
        let counts = [
            tokens.input,
            tokens.output,
            tokens.cache_read,
            tokens.cache_write,
        ];
        let mut cost = 0.;
        let mut missing = false;
        for (count, rate) in counts.into_iter().zip(rates) {
            if count > 0 {
                if let Some(rate) = rate {
                    cost += count as f64 * rate;
                } else {
                    missing = true;
                }
            }
        }
        ModelEstimate {
            model: model.into(),
            tokens,
            usd: if source.is_some() && !missing && cost.is_finite() {
                Some(cost)
            } else {
                None
            },
            partial: active
                || missing
                || source.is_none()
                || (!self.remote.contains_key(model)
                    && public_model(model)["tiered"].as_bool().unwrap_or(false)),
            source,
            rates: rates.map(|r| r.map(|n| n * 1e6)),
        }
    }
}
fn fields(v: &Value) -> [Option<f64>; 4] {
    [
        "input_cost_per_token",
        "output_cost_per_token",
        "cache_read_input_token_cost",
        "cache_creation_input_token_cost",
    ]
    .map(|k| v[k].as_f64().filter(|f| f.is_finite() && *f >= 0.))
}
fn public_catalogue() -> &'static Value {
    static CATALOGUE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CATALOGUE
        .get_or_init(|| serde_json::from_str(include_str!("prices.json")).unwrap_or(Value::Null))
}
fn public_model(model: &str) -> &Value {
    let model = model
        .strip_prefix("openai/")
        .or_else(|| model.strip_prefix("anthropic/"))
        .unwrap_or(model);
    &public_catalogue()["models"][model]
}
fn public_rates(model: &str) -> Option<[Option<f64>; 4]> {
    let value = public_model(model);
    value.is_object().then(|| fields(value))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_input_is_not_billed_twice() {
        let p = Prices::default().estimate(
            "gpt-5",
            TokenUsage {
                input: 100,
                output: 10,
                cache_read: 900,
                cache_write: 0,
            },
            false,
        );
        assert!((p.usd.unwrap() - 0.0003375).abs() < 1e-10);
    }
    #[test]
    fn unknown_and_missing_cache_prices_are_not_zero() {
        assert!(Prices::default()
            .estimate("private", TokenUsage::default(), false)
            .usd
            .is_none());
        assert!(Prices::default()
            .estimate(
                "gpt-5",
                TokenUsage {
                    cache_write: 1,
                    ..Default::default()
                },
                false
            )
            .usd
            .is_none());
    }
    #[test]
    fn ambiguous_deployments_do_not_override_public_rates() {
        let v = serde_json::json!({"data":[{"model_name":"custom","model_info":{"input_cost_per_token":1.,"output_cost_per_token":2.}},{"model_name":"custom","model_info":{"input_cost_per_token":2.,"output_cost_per_token":3.}}]});
        assert!(Prices::from_litellm(&v).remote.is_empty());
    }
}

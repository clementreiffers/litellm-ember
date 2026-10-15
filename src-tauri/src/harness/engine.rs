//! Réducteur commun. Les compteurs sont indexés par réponse, jamais additionnés à chaque lecture.
use super::pricing::Prices;
use serde_json::Value;
use shared::{Agent, AgentState, ContextUsage, Harness, ModelEstimate, SkillUse, TokenUsage};
use std::collections::{BTreeMap, BTreeSet};

const MAX_SKILLS: usize = 64;
const MAX_CONTEXT_POINTS: usize = 48;

pub fn non_empty(s: String) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}
pub fn string(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
pub fn text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_owned();
    }
    v.as_array()
        .map(|items| {
            items
                .iter()
                .filter(|i| {
                    matches!(
                        i["type"].as_str(),
                        Some("text" | "input_text" | "output_text")
                    )
                })
                .filter_map(|i| i["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
pub fn key(h: Harness, id: &str) -> String {
    format!("{}:{id}", h.key())
}
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub struct Entry {
    pub agent: Agent,
    pub owner: Option<u32>,
    pub touched: i64,
    pub order: i64,
    last_hook_at: i64,
    pub task: String,
    pub evidence: bool,
    custom_title: bool,
    /// Le processus du harnais est toujours ouvert : la session reste visible même terminée.
    pub open: bool,
    pub partial_usage: bool,
    pub origin: String,
    /// Une source de compteurs par connecteur : pas de mélange entre totaux et deltas.
    pub usage: BTreeMap<String, (String, TokenUsage)>,
}
#[derive(Default)]
pub struct Engine {
    pub entries: BTreeMap<String, Entry>,
}
impl Engine {
    pub fn entry(&mut self, h: Harness, id: &str) -> &mut Entry {
        self.entries.entry(key(h, id)).or_insert_with(|| Entry {
            agent: Agent {
                id: key(h, id),
                parent_id: None,
                harness: h,
                project: String::new(),
                title: None,
                prompt: String::new(),
                agent_path: None,
                nickname: None,
                role: None,
                prompt_unavailable: false,
                model: None,
                state: AgentState::Unknown,
                started_at: now(),
                tool: None,
                skills: vec![],
                estimates: vec![],
                context: None,
            },
            owner: None,
            touched: now(),
            order: 0,
            last_hook_at: 0,
            task: String::new(),
            evidence: false,
            custom_title: false,
            open: false,
            partial_usage: false,
            origin: String::new(),
            usage: BTreeMap::new(),
        })
    }
    pub fn snapshot(&self, prices: &Prices) -> Vec<Agent> {
        let mut retained: BTreeSet<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.open || e.agent.state.active())
            .map(|(id, _)| id.clone())
            .collect();
        // Remonte jusqu'aux parents (tolère les liens cassés et les cycles).
        loop {
            let before = retained.len();
            for id in retained.clone() {
                if let Some(parent) = self
                    .entries
                    .get(&id)
                    .and_then(|e| e.agent.parent_id.clone())
                {
                    retained.insert(parent);
                }
            }
            if before == retained.len() {
                break;
            }
        }
        // Conserve les enfants terminés des tâches encore visibles.
        loop {
            let before = retained.len();
            for (id, e) in &self.entries {
                if e.agent
                    .parent_id
                    .as_ref()
                    .is_some_and(|p| retained.contains(p))
                {
                    retained.insert(id.clone());
                }
            }
            if before == retained.len() {
                break;
            }
        }
        self.entries
            .iter()
            .filter(|(id, _)| retained.contains(*id))
            .map(|(_, e)| {
                let mut agent = e.agent.clone();
                let mut models = BTreeMap::<String, TokenUsage>::new();
                for (model, u) in e.usage.values() {
                    let total = models.entry(model.clone()).or_default();
                    total.input = total.input.saturating_add(u.input);
                    total.output = total.output.saturating_add(u.output);
                    total.cache_read = total.cache_read.saturating_add(u.cache_read);
                    total.cache_write = total.cache_write.saturating_add(u.cache_write);
                }
                if models.is_empty() {
                    agent.estimates = vec![ModelEstimate {
                        model: agent
                            .model
                            .clone()
                            .unwrap_or_else(|| "Modèle inconnu".into()),
                        tokens: TokenUsage::default(),
                        usd: None,
                        partial: true,
                        source: None,
                        rates: [None; 4],
                    }];
                } else {
                    agent.estimates = models
                        .into_iter()
                        .map(|(model, tokens)| {
                            prices.estimate(&model, tokens, agent.state.active() || e.partial_usage)
                        })
                        .collect();
                }
                agent
            })
            .collect()
    }
    pub fn reconcile(&mut self, live: &BTreeSet<u32>) {
        for e in self.entries.values_mut() {
            e.open = e.owner.is_some_and(|pid| live.contains(&pid));
            if let Some(pid) = e.owner {
                if !live.contains(&pid) && e.agent.state.active() {
                    e.agent.state = AgentState::Interrupted;
                    e.agent.tool = None;
                }
            } else if !e.origin.starts_with("http://")
                && e.agent.state.active()
                && now() - e.touched > 30
            {
                e.agent.state = AgentState::Unknown;
            }
        }
        // Évite de conserver un historique de prompts en mémoire après la fin de la tâche.
        let visible: BTreeSet<_> = self
            .snapshot(&Prices::default())
            .into_iter()
            .map(|a| a.id)
            .collect();
        self.entries.retain(|id, e| {
            if !visible.contains(id) {
                e.agent.prompt.clear();
                e.agent.skills.clear();
                e.usage.clear();
            }
            visible.contains(id) || (e.agent.parent_id.is_none() && now() - e.touched < 60)
        });
    }
    pub fn hook(&mut self, h: Harness, v: &Value, owner: Option<u32>) {
        let session = string(v, "session_id");
        if session.is_empty() {
            return;
        }
        let event = string(v, "hook_event_name");
        let child = string(v, "agent_id");
        let id = if !child.is_empty() { &child } else { &session };
        let e = self.entry(h, id);
        if let Some(at) = v["_ember_at"].as_i64() {
            if at < e.last_hook_at {
                return;
            }
            e.last_hook_at = at;
        }
        e.owner = owner.or(e.owner);
        e.evidence = true;
        e.touched = now();
        e.origin = "hooks".into();
        e.order = now();
        if !string(v, "cwd").is_empty() {
            e.agent.project = string(v, "cwd");
        }
        if id != &session {
            e.agent.parent_id = Some(key(h, &session));
        }
        match event.as_str() {
            "UserPromptSubmit" => {
                e.begin(&format!("hook:{}", uuid::Uuid::new_v4()), now());
                e.agent.prompt = string(v, "prompt");
            }
            "SubagentStart" => {
                e.begin(&format!("child:{child}"), now());
                e.agent.model = v["model"].as_str().map(str::to_owned);
            }
            "PermissionRequest" | "Elicitation" => e.agent.state = AgentState::Waiting,
            "Notification"
                if matches!(
                    v["notification_type"].as_str(),
                    Some("permission_prompt" | "elicitation_dialog")
                ) =>
            {
                e.agent.state = AgentState::Waiting
            }
            "PreToolUse" => {
                e.agent.state = if matches!(
                    v["tool_name"].as_str(),
                    Some("AskUserQuestion" | "request_user_input")
                ) {
                    AgentState::Waiting
                } else {
                    AgentState::Running
                };
                e.agent.tool = Some(string(v, "tool_name"));
                e.skill(&string(v, "tool_name"), &v["tool_input"]);
            }
            "PostToolUse" | "PostToolUseFailure" | "ElicitationResult" => {
                e.agent.state = AgentState::Running;
                e.agent.tool = None;
            }
            "Stop" | "SubagentStop" => {
                e.agent.state = AgentState::Completed;
                e.agent.tool = None;
            }
            "StopFailure" | "SessionEnd" | "Interrupt" => {
                e.agent.state = AgentState::Interrupted;
                e.agent.tool = None;
            }
            _ => {}
        }
    }
    pub fn codex(&mut self, id: &mut String, v: &Value, live: bool, owner: Option<u32>) {
        let p = &v["payload"];
        let kind = string(v, "type");
        if kind == "session_meta" {
            let declared = string(p, "id");
            if id.is_empty() {
                *id = declared;
            } else if *id != declared {
                // Un fork contient aussi le session_meta hérité du parent.
                return;
            }
        }
        if p["thread_id"].as_str().is_some_and(|thread| thread != id) {
            return;
        }
        if id.is_empty() {
            return;
        }
        let e = self.entry(Harness::Codex, id);
        e.owner = owner.or(e.owner);
        e.evidence |= live;
        if live {
            e.touched = now();
        }
        let timestamp = v["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or(e.order);
        if kind == "session_meta" {
            e.agent.project = string(p, "cwd");
            let spawn = &p["source"]["subagent"]["thread_spawn"];
            if let Some(parent) = p["parent_thread_id"]
                .as_str()
                .or_else(|| spawn["parent_thread_id"].as_str())
            {
                e.agent.parent_id = Some(key(Harness::Codex, parent));
            }
            e.agent.agent_path = non_empty(string(p, "agent_path"))
                .or_else(|| non_empty(string(spawn, "agent_path")));
            e.agent.nickname = non_empty(string(p, "agent_nickname"))
                .or_else(|| non_empty(string(spawn, "agent_nickname")));
            e.agent.role = non_empty(string(p, "agent_role"))
                .or_else(|| non_empty(string(spawn, "agent_role")));
        }
        if kind == "turn_context" {
            e.agent.model = p["model"].as_str().map(str::to_owned);
        }
        if kind == "event_msg" && timestamp >= e.order {
            e.order = timestamp;
            match p["type"].as_str().unwrap_or_default() {
                "task_started" => {
                    e.begin(&string(p, "turn_id"), timestamp);
                    if !e.evidence {
                        e.agent.state = AgentState::Unknown;
                    }
                }
                "task_complete" | "task_completed" => {
                    e.agent.state = AgentState::Completed;
                    e.agent.tool = None;
                }
                "turn_aborted" => {
                    e.agent.state = AgentState::Interrupted;
                    e.agent.tool = None;
                }
                "user_message" => {
                    e.agent.prompt = bounded_text(&string(p, "message"));
                    e.agent.prompt_unavailable = false;
                }
                _ => {}
            }
        }
        if timestamp < e.agent.started_at && kind != "session_meta" && kind != "turn_context" {
            return;
        }
        if kind == "response_item" {
            match p["type"].as_str().unwrap_or_default() {
                "message" if p["role"] == "user" => {
                    let t = text(&p["content"]);
                    // Les instructions et l'environnement précèdent le vrai prompt.
                    if !t.starts_with("# AGENTS.md instructions")
                        && !t.starts_with("<environment_context>")
                        && !t.is_empty()
                    {
                        e.agent.prompt = bounded_text(&t);
                        e.agent.prompt_unavailable = false;
                    }
                }
                "agent_message" if e.agent.agent_path.as_deref() == p["recipient"].as_str() => {
                    let content = text(&p["content"]);
                    // Les messages de suivi informatifs ne remplacent pas la mission.
                    if content.starts_with("Message Type: NEW_TASK") {
                        let payload = content
                            .split_once("Payload:\n")
                            .map(|(_, s)| s)
                            .unwrap_or(&content);
                        e.agent.prompt = bounded_text(payload);
                        e.agent.prompt_unavailable = p["content"].as_array().is_some_and(|items| {
                            items.iter().any(|item| item["type"] == "encrypted_content")
                        });
                    }
                }
                "function_call" | "custom_tool_call" => {
                    if e.agent.state.active() {
                        e.agent.tool = Some(string(p, "name"));
                        if string(p, "name").ends_with("request_user_input") {
                            e.agent.state = AgentState::Waiting;
                        }
                    }
                    let input = p["arguments"]
                        .as_str()
                        .or(p["input"].as_str())
                        .map(|s| {
                            serde_json::from_str::<Value>(s)
                                .unwrap_or_else(|_| Value::String(s.to_owned()))
                        })
                        .unwrap_or_default();
                    e.skill(&string(p, "name"), &input);
                }
                "function_call_output" | "custom_tool_call_output" => e.agent.tool = None,
                _ => {}
            }
        }
        if kind == "event_msg" && p["type"] == "token_count" {
            let last = &p["info"]["last_token_usage"];
            if last.is_object() {
                e.context(
                    n(last, "input_tokens"),
                    p["info"]["model_context_window"].as_u64(),
                );
            }
        }
        // token_usage_record est par réponse. Les token_count cumulatifs ne sont pas additionnés.
        if kind == "token_usage_record" {
            let response = string(p, "response_id");
            let model = e.agent.model.clone().unwrap_or_default();
            if !response.is_empty() {
                e.usage.insert(response, (model, openai_usage(&p["usage"])));
            }
        }
    }
    pub fn claude(&mut self, v: &Value, live: bool, owner: Option<u32>, child: Option<&str>) {
        let session = string(v, "sessionId");
        if session.is_empty() {
            return;
        }
        let id = child.unwrap_or(&session);
        let e = self.entry(Harness::Claude, id);
        e.owner = owner.or(e.owner);
        e.evidence |= live;
        if live {
            e.touched = now();
        }
        if !string(v, "cwd").is_empty() {
            e.agent.project = string(v, "cwd");
        }
        if child.is_some() {
            e.agent.parent_id = Some(key(Harness::Claude, &session));
        }
        let timestamp = v["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or(e.order);
        if timestamp < e.order {
            return;
        }
        e.order = timestamp;
        let message = &v["message"];
        match v["type"].as_str().unwrap_or_default() {
            // Un titre choisi par l'utilisateur l'emporte sur le titre généré.
            "custom-title" => {
                if let Some(title) = non_empty(string(v, "customTitle")) {
                    e.agent.title = Some(title);
                    e.custom_title = true;
                }
            }
            "ai-title" if !e.custom_title => {
                if let Some(title) = non_empty(string(v, "aiTitle")) {
                    e.agent.title = Some(title);
                }
            }
            "user"
                if !v["isMeta"].as_bool().unwrap_or(false) && v.get("toolUseResult").is_none() =>
            {
                let prompt = text(&message["content"]);
                if !prompt.is_empty() {
                    e.begin(
                        &string(v, "uuid"),
                        v["timestamp"]
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|d| d.timestamp())
                            .unwrap_or_else(now),
                    );
                    e.agent.prompt = prompt;
                    if !e.evidence {
                        e.agent.state = AgentState::Unknown;
                    }
                }
            }
            "assistant" => {
                e.agent.model = message["model"]
                    .as_str()
                    .map(str::to_owned)
                    .or(e.agent.model.clone());
                let response = string(message, "id");
                if message["usage"].is_object() {
                    let u = &message["usage"];
                    let used = n(u, "input_tokens")
                        + n(u, "cache_read_input_tokens")
                        + n(u, "cache_creation_input_tokens");
                    let model = e.agent.model.clone().unwrap_or_default();
                    e.context(used, claude_window(&model, used));
                }
                if !response.is_empty() && message["usage"].is_object() {
                    e.usage.insert(
                        response,
                        (
                            e.agent.model.clone().unwrap_or_default(),
                            anthropic_usage(&message["usage"]),
                        ),
                    );
                }
                if let Some(items) = message["content"].as_array() {
                    for item in items {
                        if item["type"] == "tool_use" {
                            e.skill(&string(item, "name"), &item["input"]);
                            if e.agent.state.active() {
                                e.agent.tool = Some(string(item, "name"));
                                if item["name"] == "AskUserQuestion" {
                                    e.agent.state = AgentState::Waiting;
                                }
                            }
                        }
                    }
                }
                if matches!(
                    message["stop_reason"].as_str(),
                    Some("end_turn" | "stop_sequence")
                ) {
                    e.agent.state = AgentState::Completed;
                    e.agent.tool = None;
                }
            }
            "system" if v["subtype"] == "turn_duration" => {
                e.agent.state = AgentState::Completed;
                e.agent.tool = None;
            }
            // Un /compact n'émet pas de réponse : sa taille après compactage vient de la frontière.
            "system" if v["subtype"] == "compact_boundary" => {
                if let Some(post) = v["compactMetadata"]["postTokens"].as_u64() {
                    e.context(post, None);
                }
            }
            _ => {}
        }
    }
}
impl Entry {
    /// Enregistre la taille du contexte de la dernière requête ; zéro est ignoré (réponse partielle).
    pub fn context(&mut self, used: u64, window: Option<u64>) {
        if used == 0 {
            return;
        }
        let context = self.agent.context.get_or_insert_with(ContextUsage::default);
        context.used = used;
        context.window = window.or(context.window);
        if context.history.last() != Some(&used) {
            context.history.push(used);
            if context.history.len() > MAX_CONTEXT_POINTS {
                context.history.remove(0);
            }
        }
    }
    pub fn begin(&mut self, task: &str, timestamp: i64) {
        // Un hook puis sa transcription décrivent le même prompt, pas deux tâches.
        let duplicate_hook = self.task.starts_with("hook:")
            && self.agent.state.active()
            && (self.agent.started_at - timestamp).abs() < 5;
        if self.task == task {
            return;
        }
        if !duplicate_hook {
            // Les skills et les tokens s'accumulent sur toute la session : le coût est celui de la session.
            // Les skills s'accumulent sur toute la session : un skill déjà chargé reste applicable.
            if self.agent.parent_id.is_none() {
                self.agent.prompt.clear();
                self.agent.prompt_unavailable = false;
            }
            self.agent.started_at = timestamp;
            self.agent.tool = None;
        }
        self.task = task.into();
        if !duplicate_hook {
            self.agent.state = AgentState::Running;
        }
    }
    pub fn skill(&mut self, name: &str, input: &Value) {
        let mut found = vec![];
        if name.eq_ignore_ascii_case("skill") {
            if let Some(s) = input["skill"].as_str().or(input["name"].as_str()) {
                found.push(SkillUse {
                    name: s.to_owned(),
                    explicit: true,
                });
            }
        } else {
            // Lecture directe d'un fichier, ou commande shell / script qui en mentionne un.
            let text = match input {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            found.extend(skills_in_text(&text).into_iter().map(|name| SkillUse {
                name,
                explicit: false,
            }));
        }
        for skill in found {
            if !self.agent.skills.contains(&skill) && self.agent.skills.len() < MAX_SKILLS {
                self.agent.skills.push(skill);
            }
        }
    }
}
/// Noms des dossiers parents des `SKILL.md` cités dans un texte.
fn skills_in_text(text: &str) -> Vec<String> {
    let mut names = vec![];
    for (end, _) in text.match_indices("/SKILL.md") {
        let name: String = text[..end]
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}
/// Les prompts restent en mémoire et leur taille exposée à l'UI est bornée.
fn bounded_text(text: &str) -> String {
    let mut result: String = text.chars().take(16_384).collect();
    if text.chars().count() > 16_384 {
        result.push_str("\n… (texte tronqué)");
    }
    result
}
/// Les transcriptions Claude n'indiquent pas la fenêtre : 200 k, ou 1 M dès qu'elle est dépassée.
fn claude_window(model: &str, used: u64) -> Option<u64> {
    Some(if model.contains("[1m]") || used > 200_000 {
        1_000_000
    } else {
        200_000
    })
}
fn n(v: &Value, k: &str) -> u64 {
    v[k].as_u64().unwrap_or(0)
}
pub fn openai_usage(v: &Value) -> TokenUsage {
    let cached = n(v, "cached_input_tokens").max(n(&v["input_tokens_details"], "cached_tokens"));
    let written = n(v, "cache_write_input_tokens");
    TokenUsage {
        input: n(v, "input_tokens")
            .saturating_sub(cached)
            .saturating_sub(written),
        output: n(v, "output_tokens"),
        cache_read: cached,
        cache_write: written,
    }
}
pub fn anthropic_usage(v: &Value) -> TokenUsage {
    TokenUsage {
        input: n(v, "input_tokens"),
        output: n(v, "output_tokens"),
        cache_read: n(v, "cache_read_input_tokens"),
        cache_write: n(v, "cache_creation_input_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn hook(event: &str) -> Value {
        json!({"session_id":"s","hook_event_name":event,"prompt":"Réparer le test","cwd":"/project"})
    }
    #[test]
    fn waiting_keeps_tab_visible_and_completed_stays_until_harness_closes() {
        let mut e = Engine::default();
        e.hook(Harness::Claude, &hook("UserPromptSubmit"), Some(42));
        e.hook(Harness::Claude, &hook("PermissionRequest"), Some(42));
        assert_eq!(e.snapshot(&Prices::default())[0].state, AgentState::Waiting);
        e.reconcile(&BTreeSet::from([42]));
        assert_eq!(e.snapshot(&Prices::default()).len(), 1);
        e.hook(Harness::Claude, &hook("Stop"), Some(42));
        e.reconcile(&BTreeSet::from([42]));
        let agents = e.snapshot(&Prices::default());
        assert_eq!(agents[0].state, AgentState::Completed);
        e.reconcile(&BTreeSet::new());
        assert!(e.snapshot(&Prices::default()).is_empty());
        assert!(e.entries.values().all(|e| e.agent.prompt.is_empty()));
    }
    #[test]
    fn open_session_is_listed_without_being_an_active_task() {
        let mut e = Engine::default();
        e.hook(Harness::Codex, &hook("SessionStart"), Some(42));
        assert!(e.snapshot(&Prices::default()).is_empty());
        e.reconcile(&BTreeSet::from([42]));
        let agents = e.snapshot(&Prices::default());
        assert_eq!(agents.len(), 1);
        assert!(!agents[0].state.active());
    }
    #[test]
    fn silent_live_process_is_not_expired() {
        let mut e = Engine::default();
        e.hook(Harness::Codex, &hook("UserPromptSubmit"), Some(42));
        e.entries.values_mut().next().unwrap().touched = 0;
        e.reconcile(&BTreeSet::from([42]));
        assert_eq!(e.snapshot(&Prices::default()).len(), 1);
    }
    #[test]
    fn dead_owner_interrupts_task() {
        let mut e = Engine::default();
        e.hook(Harness::Codex, &hook("UserPromptSubmit"), Some(42));
        e.reconcile(&BTreeSet::new());
        assert!(e.snapshot(&Prices::default()).is_empty());
    }
    #[test]
    fn uncorroborated_old_task_becomes_unknown() {
        let mut e = Engine::default();
        e.hook(Harness::Codex, &hook("UserPromptSubmit"), None);
        e.entries.values_mut().next().unwrap().touched = 0;
        e.reconcile(&BTreeSet::new());
        assert!(e.snapshot(&Prices::default()).is_empty());
    }
    #[test]
    fn child_keeps_finished_parent_visible_then_everything_disappears() {
        let mut e = Engine::default();
        e.hook(Harness::Claude, &hook("UserPromptSubmit"), Some(42));
        let mut child = hook("SubagentStart");
        child["agent_id"] = json!("child");
        e.hook(Harness::Claude, &child, Some(42));
        e.hook(Harness::Claude, &hook("Stop"), Some(42));
        assert_eq!(e.snapshot(&Prices::default()).len(), 2);
        child["hook_event_name"] = json!("SubagentStop");
        e.hook(Harness::Claude, &child, Some(42));
        e.reconcile(&BTreeSet::new());
        assert!(e.snapshot(&Prices::default()).is_empty());
        e.hook(Harness::Claude, &hook("UserPromptSubmit"), Some(42));
        assert_eq!(e.snapshot(&Prices::default()).len(), 1);
    }
    #[test]
    fn same_session_id_in_two_harnesses_is_isolated() {
        let mut e = Engine::default();
        for h in [Harness::Codex, Harness::Claude] {
            e.hook(h, &hook("UserPromptSubmit"), Some(42));
        }
        assert_eq!(e.snapshot(&Prices::default()).len(), 2);
    }
    #[test]
    fn codex_replayed_history_without_owner_does_not_open_tab() {
        let mut e = Engine::default();
        let mut id = "s".into();
        e.codex(&mut id,&json!({"timestamp":"2026-10-13T08:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}}),false,None);
        assert!(e.snapshot(&Prices::default()).is_empty());
    }
    #[test]
    fn codex_per_response_usage_is_deduplicated_and_reasoning_not_added() {
        let mut e = Engine::default();
        let mut id = "s".into();
        let t = chrono::Utc::now().to_rfc3339();
        e.codex(&mut id,&json!({"timestamp":t,"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}),true,Some(42));
        e.codex(
            &mut id,
            &json!({"timestamp":t,"type":"turn_context","payload":{"model":"gpt-5"}}),
            true,
            Some(42),
        );
        let usage = json!({"timestamp":t,"type":"token_usage_record","payload":{"response_id":"r","usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":100,"reasoning_output_tokens":50}}});
        e.codex(&mut id, &usage, true, Some(42));
        e.codex(&mut id, &usage, true, Some(42));
        let a = e.snapshot(&Prices::default());
        assert_eq!(
            a[0].estimates[0].tokens,
            TokenUsage {
                input: 100,
                output: 100,
                cache_read: 900,
                cache_write: 0
            }
        );
    }
    #[test]
    fn codex_encrypted_mission_is_explicit_and_survives_followup_turns() {
        let mut engine = Engine::default();
        let mut id = String::new();
        let t = chrono::Utc::now().to_rfc3339();
        for (kind, payload) in [
            (
                "session_meta",
                json!({"id":"child","parent_thread_id":"parent","source":{"subagent":{"thread_spawn":{"agent_path":"/root/dev","agent_role":"developer"}}}}),
            ),
            ("event_msg", json!({"type":"task_started","turn_id":"one"})),
            (
                "response_item",
                json!({"type":"agent_message","recipient":"/root/dev","content":[{"type":"input_text","text":"Message Type: NEW_TASK\nPayload:\n"},{"type":"encrypted_content","encrypted_content":"not-exposed"}]}),
            ),
            ("event_msg", json!({"type":"task_started","turn_id":"two"})),
            (
                "token_usage_record",
                json!({"thread_id":"parent","response_id":"wrong","usage":{"input_tokens":900}}),
            ),
        ] {
            engine.codex(
                &mut id,
                &json!({"timestamp":t,"type":kind,"payload":payload}),
                true,
                Some(42),
            );
        }
        let entry = &engine.entries["codex:child"];
        assert!(entry.agent.prompt_unavailable);
        assert!(entry.agent.prompt.is_empty());
        assert_eq!(entry.agent.role.as_deref(), Some("developer"));
        assert!(entry.usage.is_empty());
    }
    #[test]
    fn cache_writes_are_subtracted_from_codex_input() {
        assert_eq!(
            openai_usage(
                &json!({"input_tokens":100,"cached_input_tokens":20,"cache_write_input_tokens":70})
            ),
            TokenUsage {
                input: 10,
                cache_read: 20,
                cache_write: 70,
                output: 0
            }
        );
    }
    #[test]
    fn context_history_tracks_each_response_and_is_bounded() {
        let mut e = Engine::default();
        let a = e.entry(Harness::Codex, "a");
        a.context(100, Some(1000));
        a.context(100, None);
        a.context(0, None);
        a.context(300, None);
        let c = a.agent.context.clone().unwrap();
        assert_eq!(
            (c.used, c.window, c.history.clone()),
            (300, Some(1000), vec![100, 300])
        );
        assert_eq!(c.ratio(), Some(0.3));
        for i in 0..100 {
            a.context(1000 + i, None);
        }
        assert_eq!(
            a.agent.context.as_ref().unwrap().history.len(),
            MAX_CONTEXT_POINTS
        );
    }
    #[test]
    fn codex_and_claude_report_context() {
        let mut e = Engine::default();
        let t = chrono::Utc::now().to_rfc3339();
        let mut id = "s".into();
        e.codex(&mut id,&json!({"timestamp":t,"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}),true,Some(42));
        e.codex(&mut id,&json!({"timestamp":t,"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50000},"model_context_window":200000}}}),true,Some(42));
        let c = e
            .entries
            .values()
            .next()
            .unwrap()
            .agent
            .context
            .clone()
            .unwrap();
        assert_eq!(c.ratio(), Some(0.25));
        let v = json!({"type":"assistant","sessionId":"c","timestamp":t,"message":{"id":"r","model":"claude-sonnet-4-5","stop_reason":"tool_use","usage":{"input_tokens":10,"cache_read_input_tokens":99990,"cache_creation_input_tokens":0,"output_tokens":5},"content":[]}});
        e.claude(&v, true, Some(43), None);
        let c = e.entries["claude:c"].agent.context.clone().unwrap();
        assert_eq!((c.used, c.window), (100_000, Some(200_000)));
        let boundary = json!({"type":"system","subtype":"compact_boundary","sessionId":"c","timestamp":t,"compactMetadata":{"preTokens":100_000,"postTokens":9_800}});
        e.claude(&boundary, true, Some(43), None);
        let c = e.entries["claude:c"].agent.context.clone().unwrap();
        assert_eq!((c.used, c.window), (9_800, Some(200_000)));
    }
    #[test]
    fn claude_custom_title_wins_over_generated_title() {
        let mut e = Engine::default();
        let t = |kind: &str, key: &str, name: &str| json!({"type":kind,key:name,"sessionId":"s"});
        e.claude(&t("ai-title", "aiTitle", "Generated"), true, Some(1), None);
        e.claude(
            &t("custom-title", "customTitle", "Mine"),
            true,
            Some(1),
            None,
        );
        e.claude(
            &t("ai-title", "aiTitle", "Generated again"),
            true,
            Some(1),
            None,
        );
        assert_eq!(e.entries["claude:s"].agent.title.as_deref(), Some("Mine"));
    }
    #[test]
    fn skills_accumulate_across_prompts_of_the_same_session() {
        let mut e = Engine::default();
        e.hook(Harness::Claude, &hook("UserPromptSubmit"), Some(42));
        let mut tool = hook("PreToolUse");
        tool["tool_name"] = json!("Skill");
        tool["tool_input"] = json!({"skill":"rust-engineer"});
        e.hook(Harness::Claude, &tool, Some(42));
        e.hook(Harness::Claude, &hook("Stop"), Some(42));
        e.hook(Harness::Claude, &hook("UserPromptSubmit"), Some(42));
        assert_eq!(e.snapshot(&Prices::default())[0].skills.len(), 1);
    }
    #[test]
    fn skills_require_an_observed_invocation_or_read() {
        let mut e = Engine::default();
        let a = e.entry(Harness::Claude, "a");
        a.skill("Skill", &json!({"skill":"rust"}));
        a.skill("Read", &json!({"file_path":"/skills/review/SKILL.md"}));
        a.skill("Read", &json!({"file_path":"/skills/review/SKILL.md"}));
        a.skill("Bash", &json!({"command":"ls /skills"}));
        assert_eq!(a.agent.skills.len(), 2);
        a.skill(
            "exec",
            &Value::String("sed -n 1,80p /home/.codex/skills/rust-engineer/SKILL.md".into()),
        );
        assert_eq!(a.agent.skills[2].name, "rust-engineer");
        assert!(a.agent.skills[0].explicit);
        assert!(!a.agent.skills[1].explicit);
    }
    #[test]
    fn claude_response_updates_replace_usage_and_end_turn_hides_tab() {
        let mut e = Engine::default();
        let t = chrono::Utc::now().to_rfc3339();
        e.claude(&json!({"type":"user","sessionId":"s","uuid":"u","timestamp":t,"message":{"content":"Fix"}}),true,Some(42),None);
        let mut v = json!({"type":"assistant","sessionId":"s","timestamp":t,"message":{"id":"r","model":"claude-sonnet-4-5","usage":{"input_tokens":100,"output_tokens":10},"content":[]}});
        e.claude(&v, true, Some(42), None);
        v["message"]["usage"]["output_tokens"] = json!(20);
        e.claude(&v, true, Some(42), None);
        assert_eq!(
            e.snapshot(&Prices::default())[0].estimates[0].tokens.output,
            20
        );
        v["message"]["stop_reason"] = json!("end_turn");
        e.claude(&v, true, Some(42), None);
        assert!(e.snapshot(&Prices::default()).is_empty());
    }
    #[test]
    fn old_codex_completion_does_not_finish_new_task() {
        let mut e = Engine::default();
        e.hook(Harness::Codex, &hook("UserPromptSubmit"), Some(42));
        let mut id = "s".into();
        e.codex(&mut id,&json!({"timestamp":"2000-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_complete"}}),true,Some(42));
        assert_eq!(e.snapshot(&Prices::default()).len(), 1);
    }
    #[test]
    fn delayed_stop_hook_does_not_close_a_newer_prompt() {
        let mut engine = Engine::default();
        let mut start = hook("UserPromptSubmit");
        start["_ember_at"] = json!(200);
        engine.hook(Harness::Claude, &start, Some(42));
        let mut stop = hook("Stop");
        stop["_ember_at"] = json!(100);
        engine.hook(Harness::Claude, &stop, Some(42));
        assert_eq!(engine.snapshot(&Prices::default()).len(), 1);
    }
}

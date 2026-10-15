//! API de l'instance existante : lecture seule, aucune commande de génération.
use super::engine::{key, now, string, text, Engine};
use futures_util::StreamExt;
use serde_json::{json, Value};
mod discovery;
pub use discovery::{discover, LocalService};
use shared::{AgentState, Harness, TokenUsage};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::sync::mpsc;

pub fn endpoint(raw: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(raw).map_err(|_| "Adresse OpenCode invalide")?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("Utilisez une adresse HTTP locale sans chemin ni identifiants".into());
    }
    match url.host_str() {
        Some("127.0.0.1" | "[::1]") => {}
        Some("localhost") => {
            url.set_host(Some("127.0.0.1"))
                .map_err(|_| "Adresse locale invalide")?;
        }
        _ => return Err("Le serveur OpenCode doit être sur la boucle locale".into()),
    }
    Ok(url.as_str().trim_end_matches('/').into())
}
pub fn password() -> Result<Option<String>, String> {
    keyring::Entry::new("io.github.clementreiffers.ember", "opencode-local-password")
        .map_err(|_| "Trousseau indisponible")?
        .get_password()
        .map(Some)
        .or_else(|e| {
            if matches!(e, keyring::Error::NoEntry) {
                Ok(None)
            } else {
                Err("Trousseau OpenCode indisponible".into())
            }
        })
}
pub fn save_password(value: Option<String>) -> Result<(), String> {
    let entry = keyring::Entry::new("io.github.clementreiffers.ember", "opencode-local-password")
        .map_err(|_| "Trousseau indisponible")?;
    match value.filter(|s| !s.is_empty()) {
        Some(value) => entry
            .set_password(&value)
            .map_err(|_| "Enregistrement dans le Trousseau impossible".into()),
        None => entry
            .delete_credential()
            .or_else(|e| {
                if matches!(e, keyring::Error::NoEntry) {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(|_| "Suppression du secret impossible".into()),
    }
}
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    password: Option<String>,
}
impl Client {
    pub fn new(password: Option<String>) -> Result<Self, String> {
        Ok(Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(500))
                .build()
                .map_err(|_| "Client local indisponible")?,
            password,
        })
    }
    pub fn for_local_service(&self, local: Option<&LocalService>, base: &str) -> Self {
        let mut client = self.clone();
        if let Some(local) = local.filter(|s| s.base == base) {
            client.password = Some(local.password.clone());
        }
        client
    }
    fn request(&self, url: String) -> reqwest::RequestBuilder {
        let request = self.http.get(url);
        if let Some(password) = &self.password {
            request.basic_auth("opencode", Some(password))
        } else {
            request
        }
    }
    async fn get(&self, base: &str, path: &str) -> Result<Value, String> {
        let response = self
            .request(format!("{base}{path}"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|_| "Serveur OpenCode inaccessible")?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(
                "OpenCode demande un mot de passe ; renseignez-le dans les paramètres".into(),
            );
        }
        let response = response
            .error_for_status()
            .map_err(|_| "API OpenCode incompatible ou indisponible")?;
        let mut stream = response.bytes_stream();
        let mut bytes = vec![];
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "Réponse OpenCode interrompue")?;
            if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
                return Err("Réponse OpenCode trop volumineuse".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "Réponse OpenCode invalide".into())
    }
    pub async fn snapshot(
        &self,
        base: &str,
        observed: Vec<String>,
        opened: BTreeMap<String, u32>,
    ) -> Result<Vec<Session>, String> {
        match self.get(base, "/api/info").await {
            Ok(info) if info["version"].is_string() => {
                return self.snapshot_v2(base, observed, opened).await;
            }
            Err(error) if error.contains("mot de passe") => return Err(error),
            _ => {}
        }
        self.get(base, "/global/health").await?;
        let status = self.get(base, "/session/status").await?;
        let (permissions, questions) =
            tokio::join!(self.get(base, "/permission"), self.get(base, "/question"));
        let waiting: BTreeSet<String> = [permissions.ok(), questions.ok()]
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_array().cloned())
            .flatten()
            .filter_map(|v| v["sessionID"].as_str().map(str::to_owned))
            .collect();
        let map = status.as_object().ok_or("États OpenCode invalides")?;
        let mut sessions = vec![];
        let mut ids: BTreeSet<String> = map
            .iter()
            .filter(|(_, v)| matches!(v["type"].as_str(), Some("busy" | "retry")))
            .map(|(id, _)| id.clone())
            .collect();
        ids.extend(waiting.iter().cloned());
        ids.extend(observed);
        let mut visited = BTreeSet::new();
        while let Some(id) = ids.pop_first() {
            if !visited.insert(id.clone()) || !valid_id(&id) {
                continue;
            }
            if visited.len() > 128 {
                return Err("Plus de 128 sessions à suivre ; état incomplet".into());
            }
            let info = self.get(base, &format!("/session/{id}")).await?;
            let messages = self
                .get(base, &format!("/session/{id}/message?limit=100"))
                .await?;
            let state = if waiting.contains(&id) {
                AgentState::Waiting
            } else if map
                .get(&id)
                .is_some_and(|v| matches!(v["type"].as_str(), Some("busy" | "retry")))
            {
                AgentState::Running
            } else {
                AgentState::Completed
            };
            if let Some(parent) = info["parentID"].as_str() {
                if !visited.contains(parent) {
                    ids.insert(parent.to_owned());
                }
            }
            if state.active() {
                // Retrouve aussi les enfants déjà terminés au démarrage d'Ember.
                let children = self.get(base, &format!("/session/{id}/children")).await?;
                let started = messages
                    .as_array()
                    .and_then(|rows| rows.iter().rev().find(|m| m["info"]["role"] == "user"))
                    .and_then(|m| m["info"]["time"]["created"].as_i64());
                if let Some(children) = children.as_array() {
                    for child in children {
                        if started.is_some_and(|start| {
                            child["time"]["created"]
                                .as_i64()
                                .is_some_and(|t| t >= start)
                        }) {
                            if let Some(child_id) = child["id"].as_str() {
                                if !visited.contains(child_id) {
                                    ids.insert(child_id.to_owned());
                                }
                            }
                        }
                    }
                }
            }
            sessions.push(Session {
                owner: None,
                id,
                state,
                info,
                messages,
            });
        }
        Ok(sessions)
    }
    async fn snapshot_v2(
        &self,
        base: &str,
        observed: Vec<String>,
        opened: BTreeMap<String, u32>,
    ) -> Result<Vec<Session>, String> {
        let active = self.get(base, "/api/session/active").await?;
        let active = active["data"]
            .as_object()
            .ok_or("États OpenCode v2 invalides")?;
        let mut ids: BTreeSet<String> = active.keys().cloned().collect();
        ids.extend(opened.keys().cloned());
        ids.extend(observed);
        let mut visited = BTreeSet::new();
        let mut sessions = Vec::new();
        while let Some(id) = ids.pop_first() {
            if !valid_id(&id) || !visited.insert(id.clone()) {
                continue;
            }
            if visited.len() > 128 {
                return Err("Plus de 128 sessions à suivre ; état incomplet".into());
            }
            let response = self.get(base, &format!("/api/session/{id}")).await?;
            let mut info = response["data"].clone();
            if info["id"].as_str() != Some(&id) {
                return Err("Session OpenCode v2 invalide".into());
            }
            info["directory"] = info["location"]["directory"].clone();
            let response = self
                .get(base, &format!("/api/session/{id}/message?limit=100"))
                .await?;
            let rows = response["data"]
                .as_array()
                .ok_or("Messages OpenCode v2 invalides")?;
            // L'API v2 retourne les messages du plus récent au plus ancien.
            let messages = Value::Array(rows.iter().rev().filter_map(message_v2).collect());
            let permission_path = format!("/api/session/{id}/permission");
            let form_path = format!("/api/session/{id}/form");
            let (permissions, forms) =
                tokio::join!(self.get(base, &permission_path), self.get(base, &form_path),);
            let waiting = [permissions?, forms?].iter().any(|response| {
                response["data"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["state"]["status"]
                            .as_str()
                            .is_none_or(|s| s == "pending")
                    })
                })
            });
            let state = if waiting {
                AgentState::Waiting
            } else if active.contains_key(&id) {
                AgentState::Running
            } else {
                AgentState::Completed
            };
            if let Some(parent) = info["parentID"].as_str() {
                ids.insert(parent.to_owned());
            }
            sessions.push(Session {
                owner: opened.get(&id).copied(),
                id,
                state,
                info,
                messages,
            });
        }
        Ok(sessions)
    }
    pub fn events(
        &self,
        base: String,
        tx: mpsc::Sender<()>,
    ) -> tauri::async_runtime::JoinHandle<()> {
        let client = self.clone();
        tauri::async_runtime::spawn(async move {
            let path = match client.get(&base, "/api/info").await {
                Ok(info) if info["version"].is_string() => "/api/event",
                _ => "/event",
            };
            loop {
                if let Ok(response) = client.request(format!("{base}{path}")).send().await {
                    if response.status().is_success() {
                        let mut stream = response.bytes_stream();
                        while let Ok(Some(Ok(_))) =
                            tokio::time::timeout(Duration::from_secs(40), stream.next()).await
                        {
                            if tx.is_closed() {
                                return;
                            }
                            let _ = tx.try_send(());
                        }
                    }
                }
                if tx.is_closed() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    }
}
/// Adapte le format v2 au lecteur de compteurs commun, sans conserver les réponses brutes.
fn message_v2(message: &Value) -> Option<Value> {
    let role = message["type"].as_str()?;
    if !matches!(role, "user" | "assistant") {
        return None;
    }
    let mut parts = if role == "user" {
        vec![json!({"type": "text", "text": message["text"]})]
    } else {
        message["content"].as_array().cloned().unwrap_or_default()
    };
    for part in &mut parts {
        if part["type"] == "tool" {
            part["tool"] = part["name"].clone();
        }
    }
    Some(json!({"info": {
        "id": message["id"], "role": role, "time": message["time"],
        "modelID": message["model"]["id"], "tokens": message["tokens"]
    }, "parts": parts}))
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub struct Session {
    pub owner: Option<u32>,
    pub id: String,
    pub state: AgentState,
    pub info: Value,
    pub messages: Value,
}
pub fn apply(
    engine: &mut Engine,
    base: &str,
    result: Result<Vec<Session>, String>,
) -> Option<String> {
    let sessions = match result {
        Ok(s) => s,
        Err(error) => {
            for e in engine.entries.values_mut().filter(|e| e.origin == base) {
                e.agent.state = AgentState::Unknown;
            }
            return Some(error);
        }
    };
    let mut active = BTreeSet::new();
    for s in sessions {
        // L'identité du serveur isole des sessions portant le même identifiant.
        let id = format!("{base}/{}", s.id);
        active.insert(key(Harness::Opencode, &id));
        let e = engine.entry(Harness::Opencode, &id);
        e.owner = s.owner;
        e.open = s.owner.is_some();
        e.origin = base.into();
        e.touched = now();
        e.evidence = true;
        e.agent.project = string(&s.info, "directory");
        e.agent.title = super::engine::non_empty(string(&s.info, "title"));
        e.agent.model = s.info["model"]["id"].as_str().map(str::to_owned);
        e.agent.tool = None;
        e.agent.parent_id = s.info["parentID"]
            .as_str()
            .map(|p| key(Harness::Opencode, &format!("{base}/{p}")));
        let messages = s.messages.as_array().cloned().unwrap_or_default();
        e.partial_usage =
            messages.len() >= 100 && !messages.iter().any(|m| m["info"]["role"] == "user");
        let start = messages
            .iter()
            .rposition(|m| m["info"]["role"] == "user")
            .unwrap_or(0);
        for m in &messages[start..] {
            let info = &m["info"];
            if info["role"] == "user" {
                e.begin(
                    &string(info, "id"),
                    info["time"]["created"]
                        .as_i64()
                        .map(|n| n / 1000)
                        .unwrap_or_else(now),
                );
                e.agent.prompt = text(&m["parts"]);
                e.agent.model = info["model"]["modelID"]
                    .as_str()
                    .map(str::to_owned)
                    .or(e.agent.model.clone());
            } else if info["role"] == "assistant" {
                let model = string(info, "modelID");
                if !model.is_empty() {
                    e.agent.model = Some(model.clone());
                }
                let t = &info["tokens"];
                let cached = t["cache"]["read"].as_u64().unwrap_or(0);
                let write = t["cache"]["write"].as_u64().unwrap_or(0);
                if t.is_object() {
                    let used = t["input"].as_u64().unwrap_or(0) + cached + write;
                    if info["time"]["completed"].is_number() {
                        e.context(used, None);
                    }
                }
                if t.is_object() && !string(info, "id").is_empty() {
                    e.usage.insert(
                        string(info, "id"),
                        (
                            model,
                            TokenUsage {
                                input: t["input"].as_u64().unwrap_or(0),
                                output: t["output"].as_u64().unwrap_or(0),
                                cache_read: cached,
                                cache_write: write,
                            },
                        ),
                    );
                }
                if let Some(parts) = m["parts"].as_array() {
                    for p in parts {
                        if p["type"] == "tool" {
                            let tool = string(p, "tool");
                            e.skill(&tool, &p["state"]["input"]);
                            if p["state"]["status"] == "running" {
                                e.agent.tool = Some(tool);
                            } else {
                                e.agent.tool = None;
                            }
                        }
                    }
                }
            }
        }
        e.agent.state = s.state;
    }
    for (id, e) in &mut engine.entries {
        if e.origin == base && !active.contains(id) {
            e.owner = None;
            e.open = false;
            e.agent.state = AgentState::Completed;
            e.agent.tool = None;
        }
    }
    None
}
pub type Streams = BTreeMap<String, tauri::async_runtime::JoinHandle<()>>;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_password_is_only_used_for_its_own_endpoint() {
        let local = LocalService {
            base: "http://127.0.0.1:1234".into(),
            password: "service-secret".into(),
            opened: BTreeMap::new(),
        };
        let client = Client::new(Some("keychain-secret".into())).unwrap();
        assert_eq!(
            client
                .for_local_service(Some(&local), &local.base)
                .password
                .as_deref(),
            Some("service-secret")
        );
        assert_eq!(
            client
                .for_local_service(Some(&local), "http://127.0.0.1:5678")
                .password
                .as_deref(),
            Some("keychain-secret")
        );
    }

    #[tokio::test]
    async fn v2_pending_form_marks_an_active_session_as_waiting() {
        use httpmock::{Method::GET, MockServer};
        let server = MockServer::start_async().await;
        for (path, body) in [
            ("/api/info", json!({"version":"2.0.28"})),
            ("/api/session/active", json!({"data":{"ses_work":{}}})),
            (
                "/api/session/ses_work",
                json!({"data":{"id":"ses_work","location":{"directory":"/project"}}}),
            ),
            (
                "/api/session/ses_work/message",
                json!({"data":[
                    {"id":"a","type":"assistant","content":[{"type":"tool","name":"question","state":{"status":"running","input":{}}}]}
                ]}),
            ),
            ("/api/session/ses_work/permission", json!({"data":[]})),
            (
                "/api/session/ses_work/form",
                json!({"data":[{"state":{"status":"pending"}}]}),
            ),
        ] {
            server
                .mock_async(move |when, then| {
                    when.method(GET).path(path);
                    then.status(200).json_body(body);
                })
                .await;
        }
        let sessions = Client::new(None)
            .unwrap()
            .snapshot(&server.base_url(), vec![], BTreeMap::new())
            .await
            .unwrap();
        let mut engine = Engine::default();
        apply(&mut engine, &server.base_url(), Ok(sessions));
        let agents = engine.snapshot(&super::super::pricing::Prices::default());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, AgentState::Waiting);
        assert_eq!(agents[0].tool.as_deref(), Some("question"));
    }

    #[tokio::test]
    #[ignore = "Diagnostic explicite : API OpenCode locale en lecture seule, aucun prompt affiché"]
    async fn live_opencode_v2_read_only() {
        let processes = crate::harness::local::Processes::read().await;
        let local = processes
            .opencode
            .as_ref()
            .expect("Service local v2 détecté");
        assert!(!local.opened.is_empty(), "Un onglet TUI doit être ouvert");
        let client = Client::new(None)
            .unwrap()
            .for_local_service(Some(local), &local.base);
        let sessions = client
            .snapshot(&local.base, vec![], local.opened.clone())
            .await
            .unwrap();
        assert!(local
            .opened
            .keys()
            .all(|id| sessions.iter().any(|s| &s.id == id)));
        let mut engine = Engine::default();
        apply(&mut engine, &local.base, Ok(sessions));
        engine.reconcile(&processes.live);
        let agents = engine.snapshot(&super::super::pricing::Prices::default());
        assert!(!agents.is_empty());
        assert!(agents.iter().all(|a| a.harness == Harness::Opencode));
    }

    #[tokio::test]
    async fn v2_idle_open_tab_is_visible_without_importing_history() {
        use httpmock::{Method::GET, MockServer};
        let server = MockServer::start_async().await;
        for (path, body) in [
            ("/api/info", json!({"version":"2.0.28"})),
            ("/api/session/active", json!({"data":{}})),
            (
                "/api/session/ses_open",
                json!({"data":{
                    "id":"ses_open", "title":"Open tab", "location":{"directory":"/project"},
                    "model":{"id":"gpt-5"}
                }}),
            ),
            (
                "/api/session/ses_open/message",
                json!({"data":[
                    {"id":"idle","type":"idle"},
                    {"id":"a","type":"assistant","model":{"id":"gpt-5"},
                     "time":{"completed":2000},"content":[{"type":"text","text":"Done"}],
                     "tokens":{"input":100,"output":20,"cache":{"read":50,"write":0}}},
                    {"id":"u","type":"user","time":{"created":1000},"text":"Task"}
                ]}),
            ),
            ("/api/session/ses_open/permission", json!({"data":[]})),
            ("/api/session/ses_open/form", json!({"data":[]})),
        ] {
            server
                .mock_async(move |when, then| {
                    when.method(GET).path(path);
                    then.status(200).json_body(body);
                })
                .await;
        }
        let history = server
            .mock_async(|when, then| {
                when.path("/api/session");
                then.status(500);
            })
            .await;
        let client = Client::new(None).unwrap();
        let opened = BTreeMap::from([("ses_open".into(), 42)]);
        let sessions = client
            .snapshot(&server.base_url(), vec![], opened.clone())
            .await
            .unwrap();
        let mut engine = Engine::default();
        apply(&mut engine, &server.base_url(), Ok(sessions));
        engine.reconcile(&BTreeSet::from([42]));
        let agents = engine.snapshot(&super::super::pricing::Prices::default());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, AgentState::Completed);
        assert_eq!(agents[0].project, "/project");
        assert_eq!(agents[0].prompt, "Task");
        assert_eq!(agents[0].model.as_deref(), Some("gpt-5"));
        assert_eq!(agents[0].estimates[0].tokens.output, 20);
        // Le rafraîchissement ne double pas les compteurs.
        let sessions = client
            .snapshot(&server.base_url(), vec![], opened)
            .await
            .unwrap();
        apply(&mut engine, &server.base_url(), Ok(sessions));
        assert_eq!(
            engine.snapshot(&super::super::pricing::Prices::default())[0].estimates[0]
                .tokens
                .output,
            20
        );
        let sessions = client
            .snapshot(&server.base_url(), vec![], BTreeMap::new())
            .await
            .unwrap();
        assert!(sessions.is_empty());
        apply(&mut engine, &server.base_url(), Ok(sessions));
        assert!(engine
            .snapshot(&super::super::pricing::Prices::default())
            .is_empty());
        history.assert_hits_async(0).await;
    }

    #[test]
    fn endpoint_is_strictly_local_without_credentials() {
        for url in [
            "https://example.com",
            "http://example.com",
            "http://u:p@127.0.0.1:4096",
            "http://127.0.0.1:4096/path",
        ] {
            assert!(endpoint(url).is_err());
        }
        assert_eq!(
            endpoint("http://localhost:4096").unwrap(),
            "http://127.0.0.1:4096"
        );
    }
    #[test]
    fn lost_server_is_unknown_not_completed() {
        let mut e = Engine::default();
        let a = e.entry(Harness::Opencode, "a");
        a.origin = "server".into();
        a.agent.state = AgentState::Running;
        apply(&mut e, "server", Err("offline".into()));
        assert_eq!(
            e.entries.values().next().unwrap().agent.state,
            AgentState::Unknown
        );
    }
    #[tokio::test]
    async fn api_collects_waiting_parent_and_completed_child_without_writes() {
        use httpmock::{Method::GET, MockServer};
        use serde_json::json;
        let server = MockServer::start_async().await;
        let responses = [
            ("/global/health", json!({"healthy":true,"version":"1.19.0"})),
            ("/session/status", json!({"root":{"type":"busy"}})),
            ("/permission", json!([{"sessionID":"root"}])),
            ("/question", json!([])),
            ("/session/root", json!({"id":"root","directory":"/project"})),
            (
                "/session/root/children",
                json!([{"id":"child","time":{"created":2000}}]),
            ),
            (
                "/session/root/message",
                json!([
                    {"info":{"id":"u","role":"user","time":{"created":1000}},"parts":[{"type":"text","text":"Fix <script> is text"}]},
                    {"info":{"id":"r","role":"assistant","modelID":"gpt-5","tokens":{"input":100,"output":10,"cache":{"read":900,"write":0}}},"parts":[]}
                ]),
            ),
            (
                "/session/child",
                json!({"id":"child","parentID":"root","directory":"/project"}),
            ),
            (
                "/session/child/message",
                json!([
                    {"info":{"id":"cu","role":"user","time":{"created":2000}},"parts":[{"type":"text","text":"Inspect tests"}]},
                    {"info":{"id":"cr","role":"assistant","modelID":"gpt-5","tokens":{"input":50,"output":5,"cache":{"read":0,"write":0}}},"parts":[]}
                ]),
            ),
        ];
        for (path, value) in responses {
            server
                .mock_async(move |when, then| {
                    when.method(GET).path(path);
                    then.status(200).json_body(value);
                })
                .await;
        }
        let client = Client::new(None).unwrap();
        let sessions = client
            .snapshot(&server.base_url(), vec![], BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(sessions.len(), 2);
        let mut engine = Engine::default();
        apply(&mut engine, &server.base_url(), Ok(sessions));
        let agents = engine.snapshot(&super::super::pricing::Prices::default());
        assert_eq!(agents.len(), 2);
        let parent = agents.iter().find(|a| a.parent_id.is_none()).unwrap();
        assert_eq!(parent.state, AgentState::Waiting);
        assert_eq!(parent.estimates[0].tokens.input, 100);
        assert_eq!(agents.iter().filter(|a| a.state.active()).count(), 1);
        let sessions = client
            .snapshot(&server.base_url(), vec![], BTreeMap::new())
            .await
            .unwrap();
        apply(&mut engine, &server.base_url(), Ok(sessions));
        assert_eq!(
            engine
                .snapshot(&super::super::pricing::Prices::default())
                .iter()
                .map(|a| a.estimates[0].tokens.output)
                .sum::<u64>(),
            15
        );
        apply(&mut engine, &server.base_url(), Ok(vec![]));
        assert!(engine
            .snapshot(&super::super::pricing::Prices::default())
            .is_empty());
    }
    #[tokio::test]
    async fn unauthorized_server_reports_configuration_need_without_credentials() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.path("/global/health");
                then.status(401).body("private-response");
            })
            .await;
        let error = Client::new(Some("private-secret".into()))
            .unwrap()
            .snapshot(&server.base_url(), vec![], BTreeMap::new())
            .await
            .err()
            .unwrap();
        assert!(error.contains("mot de passe"));
        assert!(!error.contains("private-"));
    }
}

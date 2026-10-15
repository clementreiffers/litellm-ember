//! Observateur local indépendant de la connexion LiteLLM.
mod engine;
mod hooks;
mod local;
mod opencode;
mod pricing;

use engine::Engine;
use pricing::Prices;
use shared::{Harness, HarnessSettings, HarnessSnapshot, HarnessStatus};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tauri::Emitter;
use tokio::sync::{mpsc, watch, Mutex};

pub use hooks::collect_if_requested;
pub fn validate_settings(settings: &HarnessSettings) -> Result<(), String> {
    for h in Harness::ALL {
        let cfg = settings.get(h);
        if !cfg.root.is_empty() && !std::path::Path::new(&cfg.root).is_absolute() {
            return Err("Le répertoire du harnais doit être un chemin absolu".into());
        }
        if cfg.disabled && cfg.hooks {
            return Err("Désactivez les hooks avant de désactiver le suivi".into());
        }
        if !cfg.endpoint.is_empty() {
            if h != Harness::Opencode {
                return Err("Seul OpenCode utilise une adresse locale".into());
            }
            opencode::endpoint(&cfg.endpoint)?;
        }
        if h == Harness::Opencode && cfg.hooks {
            return Err("OpenCode utilise son API locale".into());
        }
    }
    Ok(())
}
enum Message {
    Processes(local::Processes),
    OpenCode(u64, String, Result<Vec<opencode::Session>, String>),
    Prices(u64, Prices),
    Configure(HarnessSettings),
    Credentials,
}
#[derive(Clone)]
pub struct Service {
    snapshots: watch::Receiver<HarnessSnapshot>,
    sender: mpsc::Sender<Message>,
    configuration: Arc<Mutex<()>>,
    settings: crate::service::Service,
}
impl Service {
    pub fn start(app: tauri::AppHandle, settings: crate::service::Service) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        let (publish, snapshots) = watch::channel(HarnessSnapshot::default());
        let service = Self {
            snapshots,
            sender: sender.clone(),
            configuration: Arc::new(Mutex::new(())),
            settings: settings.clone(),
        };
        tauri::async_runtime::spawn(run(app, settings, sender, receiver, publish));
        service
    }
    pub fn snapshot(&self) -> HarnessSnapshot {
        self.snapshots.borrow().clone()
    }
    pub async fn settings(&self) -> Result<HarnessSettings, String> {
        self.settings.harness_settings().await
    }
    pub async fn configure(&self, input: HarnessSettings) -> Result<HarnessSettings, String> {
        validate_settings(&input)?;
        let _guard = self.configuration.lock().await;
        self.settings.save_harness_settings(input.clone()).await?;
        self.sender
            .send(Message::Configure(input.clone()))
            .await
            .map_err(|_| "Suivi indisponible")?;
        Ok(input)
    }
    pub async fn password(&self, value: Option<String>) -> Result<(), String> {
        tauri::async_runtime::spawn_blocking(move || opencode::save_password(value))
            .await
            .map_err(|_| "Trousseau indisponible")??;
        self.sender
            .send(Message::Credentials)
            .await
            .map_err(|_| "Suivi indisponible".into())
    }
}
async fn run(
    app: tauri::AppHandle,
    settings: crate::service::Service,
    tx: mpsc::Sender<Message>,
    mut rx: mpsc::Receiver<Message>,
    publish: watch::Sender<HarnessSnapshot>,
) {
    let mut cfg = match settings.harness_settings().await {
        Ok(cfg) => cfg,
        Err(_) => return,
    };
    let (hook_tx, mut hook_rx) = mpsc::channel(256);
    let socket_error = hooks::listen(hook_tx).err();
    let (event_tx, mut events) = mpsc::channel(1);
    let mut streams = opencode::Streams::new();
    let mut api = load_client().await;
    let mut processes = local::Processes::default();
    let mut engine = Engine::default();
    let mut reader = local::Reader::default();
    let mut statuses = vec![];
    let mut prices = Prices::default();
    let mut prices_generation = settings.stats().generation;
    let mut prices_due = Instant::now();
    let mut scanning = false;
    let mut pending = BTreeSet::new();
    let mut next_poll = BTreeMap::<String, Instant>::new();
    let mut configuration_generation = 0u64;
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _=timer.tick()=>{
                if !scanning{
                    scanning=true;let tx=tx.clone();
                    tauri::async_runtime::spawn(async move{let p=local::Processes::read().await;let _=tx.send(Message::Processes(p)).await;});
                }
                if settings.stats().generation!=prices_generation{prices=Prices::default();prices_generation=settings.stats().generation;prices_due=Instant::now();}
                if Instant::now()>=prices_due{
                    prices_due=Instant::now()+Duration::from_secs(300);
                    let settings=settings.clone();let tx=tx.clone();
                    tauri::async_runtime::spawn(async move{if let Ok((generation,value))=settings.harness_prices().await{let _=tx.send(Message::Prices(generation,Prices::from_litellm(&value))).await;}});
                }
            }
            Some(hook)=hook_rx.recv()=>{
                if !cfg.get(hook.harness).disabled&&cfg.get(hook.harness).hooks{
                    let owner=processes.owner(hook.parent,hook.harness);
                    engine.hook(hook.harness,&hook.payload,owner);
                }
            }
            Some(_)=events.recv()=>{}
            Some(message)=rx.recv()=>match message{
                Message::Configure(next)=>{
                    cfg=next;reader=local::Reader::default();engine=Engine::default();
                    configuration_generation+=1;pending.clear();next_poll.clear();
                    for (_,task) in std::mem::take(&mut streams){task.abort();}
                }
                Message::Credentials=>{configuration_generation+=1;pending.clear();next_poll.clear();api=load_client().await;for (_,task) in std::mem::take(&mut streams){task.abort();}}
                Message::Prices(generation,next)=>{if generation==settings.stats().generation{prices=next;}}
                Message::OpenCode(generation,base,result)=>{
                    if generation != configuration_generation { continue; }
                    pending.remove(&base);
                    if !cfg.opencode.disabled && (processes.servers.contains(&base) || opencode::endpoint(&cfg.opencode.endpoint).is_ok_and(|url|url==base)){
                        let error=opencode::apply(&mut engine,&base,result);
                        if let Some(s)=statuses.iter_mut().find(|s:&&mut HarnessStatus|s.harness==Harness::Opencode){s.available=error.is_none();s.coverage="API locale · états, messages, outils et tokens".into();s.error=error;}
                    }
                }
                Message::Processes(next)=>{
                    processes=next;scanning=false;
                    // L'I/O fichier reste hors du runtime async ; un seul lecteur possède ses offsets.
                    let local_cfg=cfg.clone();
                    let (returned_reader,returned_engine,returned_processes,next_statuses)=match tauri::async_runtime::spawn_blocking(move||{
                        let statuses=reader.scan(&local_cfg,&processes,&mut engine);
                        (reader,engine,processes,statuses)
                    }).await{Ok(result)=>result,Err(_)=>return};
                    reader=returned_reader;engine=returned_engine;processes=returned_processes;
                    let old_opencode=statuses.iter().find(|s:&&HarnessStatus|s.harness==Harness::Opencode).cloned();
                    statuses=next_statuses;
                    if let Some(old)=old_opencode{if let Some(s)=statuses.iter_mut().find(|s|s.harness==Harness::Opencode){if !cfg.opencode.disabled{*s=old;}}}
                    if processes.available { engine.reconcile(&processes.live); } else {
                        reader=local::Reader::default();
                        for e in engine.entries.values_mut().filter(|e|!e.origin.starts_with("http://")) {e.agent.state=shared::AgentState::Unknown;}
                        for s in &mut statuses {s.available=false;s.error=Some("Inspection des processus indisponible".into());}
                    }
                }
            },
            else=>break,
        }
        let mut endpoints = if cfg.opencode.disabled {
            BTreeSet::new()
        } else {
            processes.servers.clone()
        };
        if !cfg.opencode.disabled && !cfg.opencode.endpoint.is_empty() {
            if let Ok(url) = opencode::endpoint(&cfg.opencode.endpoint) {
                endpoints.insert(url);
            }
        }
        let removed: Vec<_> = streams
            .keys()
            .filter(|base| !endpoints.contains(*base))
            .cloned()
            .collect();
        for base in removed {
            if let Some(task) = streams.remove(&base) {
                task.abort();
            }
            let _ = opencode::apply(&mut engine, &base, Err("Connexion perdue".into()));
        }
        if let Ok(client) = &api {
            for base in &endpoints {
                let client = client.for_local_service(processes.opencode.as_ref(), base);
                if !streams.contains_key(base) {
                    streams.insert(base.clone(), client.events(base.clone(), event_tx.clone()));
                }
                if !pending.contains(base)
                    && next_poll
                        .get(base)
                        .is_none_or(|deadline| Instant::now() >= *deadline)
                {
                    pending.insert(base.clone());
                    next_poll.insert(base.clone(), Instant::now() + Duration::from_secs(1));
                    let generation = configuration_generation;
                    let prefix = format!("opencode:{base}/");
                    let observed = engine
                        .entries
                        .values()
                        .filter(|e| e.origin == *base && e.agent.state.active())
                        .filter_map(|e| e.agent.id.strip_prefix(&prefix).map(str::to_owned))
                        .collect();
                    let opened = processes
                        .opencode
                        .as_ref()
                        .filter(|local| local.base == *base)
                        .map(|local| local.opened.clone())
                        .unwrap_or_default();
                    let client = client.clone();
                    let base = base.clone();
                    let tx = tx.clone();
                    tauri::async_runtime::spawn(async move {
                        let result = tokio::time::timeout(
                            Duration::from_secs(10),
                            client.snapshot(&base, observed, opened),
                        )
                        .await
                        .unwrap_or_else(|_| Err("Lecture OpenCode expirée".into()));
                        let _ = tx.send(Message::OpenCode(generation, base, result)).await;
                    });
                }
            }
        }
        for s in &mut statuses {
            if cfg.get(s.harness).disabled {
                s.available = false;
                s.error = None;
                s.coverage = "Désactivé".into();
                continue;
            }
            if local::has_unknown(&engine, s.harness) {
                s.error=Some("Certaines tâches ont un état inconnu ; elles ne sont pas comptées comme actives".into());
            }
            if cfg.get(s.harness).hooks {
                if let Some(error) = &socket_error {
                    s.error = Some(error.clone());
                }
            }
            if s.harness == Harness::Opencode && endpoints.is_empty() {
                s.available = false;
                s.coverage = "Aucun serveur local actif détecté".into();
            }
            if s.harness == Harness::Opencode {
                if let Err(error) = &api {
                    s.error = Some(error.clone());
                }
            }
        }
        let previous = publish.borrow().clone();
        let mut next = HarnessSnapshot {
            revision: previous.revision,
            agents: engine.snapshot(&prices),
            statuses: statuses.clone(),
        };
        if next != previous {
            next.revision += 1;
            publish.send_replace(next.clone());
            let _ = app.emit("harness-updated", next);
        }
    }
    for (_, task) in streams {
        task.abort();
    }
}
async fn load_client() -> Result<opencode::Client, String> {
    let password = tauri::async_runtime::spawn_blocking(opencode::password)
        .await
        .map_err(|_| "Trousseau indisponible")??;
    opencode::Client::new(password)
}

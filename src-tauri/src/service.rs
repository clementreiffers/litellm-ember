//! Coordinateur : seul propriétaire de l'état publié. Les résultats portent un cycle,
//! et les sauvegardes interrompent ce cycle avant de changer la configuration.
use crate::{
    litellm::{Client, KeyData},
    notify::Notifier,
    settings::{Settings, SettingsStore},
};
use chrono::Local;
use shared::{
    Activity, Detail, ModelCost, ModelUsage, SettingsInput, SettingsView, Stats, WeekDetails,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Service de rafraîchissement indisponible")]
    Closed,
    #[error("La configuration a changé ; rechargez cet onglet")]
    Obsolete,
}

type NotifyBudget = Arc<dyn Fn(Settings, KeyData) + Send + Sync>;

type Reply<T> = oneshot::Sender<Result<T, String>>;
enum Command {
    Save(SettingsInput, Reply<SettingsView>),
    Settings(Reply<SettingsView>),
    Context(Reply<(Client, Stats)>),
    Refresh(Option<Reply<()>>),
    Event(u64, Event),
}

pub enum Event {
    Key(KeyData),
    Models(Vec<ModelCost>),
    Today(String, Vec<ModelUsage>),
    Failed(String),
    Finished,
}

/// Transition déterministe ; les effets sont traités par le coordinateur.
pub fn apply(stats: &mut Stats, event: Event) {
    match event {
        Event::Key(key) => {
            stats.total_spend = key.period_spend;
            stats.period_spend = key.period_spend;
            stats.max_budget = key.max_budget;
            stats.budget_reset_at = key.budget_reset_at;
            stats.period_start = key.period_start;
        }
        Event::Models(models) => stats.models_total = models,
        Event::Today(date, rows) => {
            stats.today_date = Some(date);
            stats.today = rows;
        }
        Event::Failed(error) => stats.error = Some(error),
        Event::Finished => stats.refreshing = false,
    }
}

pub async fn run_cycle(client: &Client, publish: impl Fn(Event) + Sync) {
    let key = match client.fetch_key().await {
        Ok(key) => key,
        Err(error) => {
            publish(Event::Failed(error));
            publish(Event::Finished);
            return;
        }
    };
    let start = key.period_start.clone();
    publish(Event::Key(key));
    let models = async {
        publish(match client.fetch_models(start.as_deref()).await {
            Ok(rows) => Event::Models(rows),
            Err(error) => Event::Failed(error),
        });
    };
    let today = async {
        publish(match client.fetch_today_snapshot().await {
            Ok((day, rows)) => Event::Today(day.to_string(), rows),
            Err(error) => Event::Failed(error),
        });
    };
    tokio::join!(models, today);
    publish(Event::Finished);
}

#[derive(Clone)]
pub struct Service {
    commands: mpsc::UnboundedSender<Command>,
    snapshots: watch::Receiver<Stats>,
}
impl Service {
    pub fn start(
        settings_path: Option<PathBuf>,
        cache: Option<PathBuf>,
        notification_path: Option<PathBuf>,
        app: tauri::AppHandle,
        publish: impl Fn(&Stats) + Send + 'static,
    ) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (snapshots, updates) = watch::channel(Stats::default());
        let service = Self {
            commands: commands.clone(),
            snapshots: updates,
        };
        tauri::async_runtime::spawn(async move {
            let loaded = tauri::async_runtime::spawn_blocking(move || {
                (
                    Arc::new(SettingsStore::load(settings_path)),
                    Arc::new(Notifier::load(notification_path)),
                )
            })
            .await;
            match loaded {
                Ok((store, notifier)) => {
                    let notify: NotifyBudget = Arc::new(move |cfg, key| {
                        notifier.check(
                            &app,
                            &cfg,
                            key.period_spend,
                            key.max_budget,
                            key.budget_reset_at.as_deref(),
                        )
                    });
                    coordinate(store, cache, notify, publish, commands, receiver, snapshots).await
                }
                Err(_) => {
                    let state = Stats {
                        revision: 1,
                        error: Some("Initialisation du stockage interrompue".into()),
                        ..Default::default()
                    };
                    snapshots.send_replace(state.clone());
                    publish(&state);
                }
            }
        });
        service
    }
    pub fn stats(&self) -> Stats {
        self.snapshots.borrow().clone()
    }
    pub fn refresh(&self) {
        let _ = self.commands.send(Command::Refresh(None));
    }
    pub async fn refresh_and_wait(&self) -> Result<(), String> {
        self.request(|reply| Command::Refresh(Some(reply))).await
    }
    async fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| Error::Closed.to_string())?;
        result.await.map_err(|_| Error::Closed.to_string())?
    }
    pub async fn settings(&self) -> Result<SettingsView, String> {
        self.request(Command::Settings).await
    }
    pub async fn save(&self, input: SettingsInput) -> Result<SettingsView, String> {
        self.request(|reply| Command::Save(input, reply)).await
    }
    pub async fn week(&self) -> Result<Detail<WeekDetails>, String> {
        let (client, state) = self.request(Command::Context).await?;
        let mut updates = self.snapshots.clone();
        tokio::select! {
            result = client.fetch_week(state.period_start.as_deref(), state.budget_reset_at.as_deref(), state.period_spend) => self.detail(state.generation, result?),
            _ = updates.wait_for(|snapshot| snapshot.generation != state.generation) => Err(Error::Obsolete.to_string()),
        }
    }
    pub async fn activity(&self) -> Result<Detail<Activity>, String> {
        let (client, state) = self.request(Command::Context).await?;
        let mut updates = self.snapshots.clone();
        tokio::select! {
            result = client.fetch_activity() => self.detail(state.generation, result?),
            _ = updates.wait_for(|snapshot| snapshot.generation != state.generation) => Err(Error::Obsolete.to_string()),
        }
    }
    fn detail<T>(&self, generation: u64, data: T) -> Result<Detail<T>, String> {
        if generation != self.snapshots.borrow().generation {
            return Err(Error::Obsolete.to_string());
        }
        Ok(Detail { generation, data })
    }
}

fn connection(
    store: &SettingsStore,
    http: &Result<reqwest::Client, String>,
) -> Result<Client, String> {
    if let Some(error) = store.fault() {
        return Err(error);
    }
    let key = store
        .api_key()
        .ok_or("Aucune clé API : renseignez-la dans les paramètres.")?;
    Client::with_http(&store.get().base_url, key, http.clone()?)
}

async fn coordinate(
    store: Arc<SettingsStore>,
    cache: Option<PathBuf>,
    notify: NotifyBudget,
    publish: impl Fn(&Stats) + Send + 'static,
    commands: mpsc::UnboundedSender<Command>,
    mut receiver: mpsc::UnboundedReceiver<Command>,
    snapshots: watch::Sender<Stats>,
) {
    let http = Client::http();
    let mut client = connection(&store, &http);
    let source = store.get().source_id;
    let cache_read = cache.clone();
    let mut stats = tauri::async_runtime::spawn_blocking(move || {
        cache_read
            .as_deref()
            .and_then(crate::persistence::load_cache)
            .filter(|s| !source.is_empty() && s.source_id == source)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    stats.source_id = store.get().source_id;
    stats.generation = 1;
    stats.revision = 0;
    stats.refreshing = false;
    let mut worker: Option<tauri::async_runtime::JoinHandle<()>> = None;
    let mut pending = true;
    let mut next = tokio::time::Instant::now();
    loop {
        if pending && worker.is_none() {
            pending = false;
            stats.cycle_id += 1;
            stats.error = None;
            if stats.today_date.as_deref()
                != Some(Local::now().format("%Y-%m-%d").to_string().as_str())
            {
                stats.today.clear();
                stats.today_date = None;
            }
            stats.refreshing = true;
            match client.clone() {
                Ok(client) => {
                    let commands = commands.clone();
                    let cycle = stats.cycle_id;
                    worker = Some(tauri::async_runtime::spawn(async move {
                        run_cycle(&client, |event| {
                            let _ = commands.send(Command::Event(cycle, event));
                        })
                        .await;
                    }));
                }
                Err(error) => {
                    stats.error = Some(error);
                    stats.refreshing = false;
                    next = tokio::time::Instant::now()
                        + Duration::from_secs(store.get().refresh_secs.clamp(5, 3600).into());
                }
            }
            stats.revision += 1;
            snapshots.send_replace(stats.clone());
            publish(&stats);
        }
        let command = tokio::select! {
            command = receiver.recv() => match command { Some(c) => c, None => break },
            _ = tokio::time::sleep_until(next), if worker.is_none() => { client = connection(&store, &http); pending = true; continue; }
        };
        match command {
            Command::Settings(reply) => {
                let _ = reply.send(Ok(store.view()));
            }
            Command::Context(reply) => {
                let _ = reply.send(client.clone().map(|c| (c, stats.clone())));
            }
            Command::Refresh(reply) => {
                client = connection(&store, &http);
                pending = true;
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
            }
            Command::Save(input, reply) => {
                if let Some(worker) = worker.take() {
                    worker.abort();
                }
                // Invalide aussi les événements déjà en file et les requêtes d'onglets.
                stats.cycle_id += 1;
                stats.generation += 1;
                stats.refreshing = false;
                stats.revision += 1;
                snapshots.send_replace(stats.clone());
                publish(&stats);
                let saved_store = store.clone();
                let saved = tauri::async_runtime::spawn_blocking(move || saved_store.save(input))
                    .await
                    .map_err(|_| "La sauvegarde a été interrompue".to_string())
                    .and_then(|r| r);
                if matches!(saved, Ok(true)) {
                    stats = Stats {
                        generation: stats.generation,
                        cycle_id: stats.cycle_id,
                        revision: stats.revision,
                        source_id: store.get().source_id,
                        ..Default::default()
                    };
                }
                client = connection(&store, &http);
                stats.revision += 1;
                snapshots.send_replace(stats.clone());
                publish(&stats);
                let _ = reply.send(saved.map(|_| store.view()));
                pending = true;
            }
            Command::Event(cycle, event) => {
                if cycle != stats.cycle_id {
                    continue;
                }
                if let Event::Key(key) = &event {
                    let notify = notify.clone();
                    let cfg = store.get();
                    let key = KeyData {
                        period_spend: key.period_spend,
                        max_budget: key.max_budget,
                        budget_reset_at: key.budget_reset_at.clone(),
                        period_start: key.period_start.clone(),
                    };
                    let _ = tauri::async_runtime::spawn_blocking(move || notify(cfg, key)).await;
                }
                if matches!(event, Event::Today(..)) {
                    stats.updated_at = Some(Local::now().format("%H:%M:%S").to_string());
                }
                let finished = matches!(event, Event::Finished);
                apply(&mut stats, event);
                stats.revision += 1;
                snapshots.send_replace(stats.clone());
                publish(&stats);
                if finished {
                    worker = None;
                    next = tokio::time::Instant::now()
                        + Duration::from_secs(store.get().refresh_secs.clamp(5, 3600).into());
                    if let Some(path) = cache.clone() {
                        let snapshot = stats.clone();
                        let _ = tauri::async_runtime::spawn_blocking(move || {
                            if let Err(e) = crate::persistence::write(&path, &snapshot) {
                                eprintln!("Cache : {e}");
                            }
                        })
                        .await;
                    }
                }
            }
        }
    }
    if let Some(worker) = worker {
        worker.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mocks(server: &MockServer, spend: f64, delay: u64) {
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(200)
                .delay(Duration::from_millis(delay))
                .json_body(serde_json::json!({"info":{"spend":spend}}));
        });
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(serde_json::json!([]));
        });
    }
    fn start(
        store: Arc<SettingsStore>,
        calls: Arc<AtomicUsize>,
        cache: Option<PathBuf>,
    ) -> (Service, tokio::task::JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (snapshots, updates) = watch::channel(Stats::default());
        let service = Service {
            commands: commands.clone(),
            snapshots: updates,
        };
        let task = tokio::spawn(coordinate(
            store,
            cache,
            Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
            }),
            |_| {},
            commands,
            receiver,
            snapshots,
        ));
        (service, task)
    }
    async fn finished(service: &Service, minimum_cycle: u64) -> Stats {
        let mut snapshots = service.snapshots.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let current = snapshots.borrow().clone();
                if current.cycle_id >= minimum_cycle && !current.refreshing && current.revision > 0
                {
                    return current;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn saved_connection_rejects_old_queued_results_and_old_tab_responses() {
        let old = MockServer::start();
        let new = MockServer::start();
        mocks(&old, 999.0, 100);
        mocks(&new, 7.0, 0);
        let calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(SettingsStore::for_test(&old.base_url()));
        let (service, task) = start(store, calls.clone(), None);
        let (_, old_state) = service.request(Command::Context).await.unwrap();
        service
            .save(SettingsInput {
                base_url: new.base_url(),
                refresh_secs: 30,
                notifications_enabled: true,
                notify_info_percent: 50,
                notify_critical_percent: 75,
                ..Default::default()
            })
            .await
            .unwrap();
        // Simule un résultat déjà en transit au moment de l'annulation réseau.
        service
            .commands
            .send(Command::Event(
                old_state.cycle_id,
                Event::Key(KeyData {
                    period_spend: 999.0,
                    max_budget: None,
                    budget_reset_at: None,
                    period_start: None,
                }),
            ))
            .unwrap();
        service
            .commands
            .send(Command::Event(old_state.cycle_id, Event::Finished))
            .unwrap();
        let state = finished(&service, old_state.cycle_id + 2).await;
        assert_eq!(state.total_spend, 7.0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(service
            .detail(old_state.generation, Activity::default())
            .is_err());
        task.abort();
    }
    #[tokio::test]
    async fn forced_refreshes_during_a_cycle_coalesce_into_one_followup() {
        let server = MockServer::start();
        mocks(&server, 1.0, 100);
        let calls = Arc::new(AtomicUsize::new(0));
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            calls.clone(),
            None,
        );
        service.request(Command::Context).await.unwrap();
        for _ in 0..5 {
            service.refresh_and_wait().await.unwrap();
        }
        let state = finished(&service, 2).await;
        assert_eq!(state.cycle_id, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }
    #[tokio::test]
    async fn failed_cycle_terminates_and_preserves_cached_amount_for_the_same_source() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(503);
        });
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        let path = std::env::temp_dir().join(format!("{}.json", uuid::Uuid::new_v4()));
        crate::persistence::write(
            &path,
            &Stats {
                source_id: store.get().source_id,
                total_spend: 42.0,
                ..Default::default()
            },
        )
        .unwrap();
        let (service, task) = start(store, Arc::new(AtomicUsize::new(0)), Some(path.clone()));
        let state = finished(&service, 1).await;
        assert_eq!(state.total_spend, 42.0);
        assert!(state.error.is_some());
        assert!(!state.refreshing);
        // La commande sert de barrière : le commit du cache précède sa réception.
        service.settings().await.unwrap();
        let saved: Stats = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.cycle_id, 1);
        task.abort();
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn caches_without_a_matching_source_are_never_displayed() {
        for source in ["", "other-source"] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/key/info");
                then.status(503);
            });
            let path = std::env::temp_dir().join(format!("{}.json", uuid::Uuid::new_v4()));
            crate::persistence::write(
                &path,
                &Stats {
                    source_id: source.into(),
                    total_spend: 999.0,
                    ..Default::default()
                },
            )
            .unwrap();
            let (service, task) = start(
                Arc::new(SettingsStore::for_test(&server.base_url())),
                Arc::new(AtomicUsize::new(0)),
                Some(path.clone()),
            );
            assert_eq!(finished(&service, 1).await.total_spend, 0.0);
            service.settings().await.unwrap();
            task.abort();
            std::fs::remove_file(path).unwrap();
        }
    }
}

//! Coordinateur : seul propriétaire de l'état publié. Les résultats portent un cycle,
//! et les sauvegardes interrompent ce cycle avant de changer la configuration.
use crate::{
    litellm::{Client, KeyData},
    notify::Notifier,
    settings::{Settings, SettingsStore},
};
use chrono::Local;
use shared::{
    Activity, Detail, HistoryDetails, HistoryMode, ModelCost, ModelUsage, SettingsInput,
    SettingsView, Stats, WeekDetails,
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

enum NotificationRequest {
    Budget(KeyData),
    IncreaseDue(chrono::NaiveDate),
    Increase(chrono::NaiveDate, crate::increase::Comparison),
}

type Notify = Arc<dyn Fn(Settings, NotificationRequest) -> Result<bool, String> + Send + Sync>;

/// Temporisation monotone, distincte des journées UTC comparées.
#[derive(Default)]
struct IncreaseRetry(Option<tokio::time::Instant>);
impl IncreaseRetry {
    fn ready(&self, now: tokio::time::Instant) -> bool {
        self.0.is_none_or(|deadline| now >= deadline)
    }
    fn failed(&mut self, now: tokio::time::Instant) {
        self.0 = Some(now + Duration::from_secs(15 * 60));
    }
}

type Reply<T> = oneshot::Sender<Result<T, String>>;
enum Command {
    Save(SettingsInput, Reply<SettingsView>),
    Settings(Reply<SettingsView>),
    History(HistoryMode, Reply<Detail<HistoryDetails>>),
    HistoryFetched(
        u64,
        chrono::DateTime<chrono::Utc>,
        chrono::NaiveDate,
        Result<Box<[Option<f64>; crate::history::DAYS]>, String>,
    ),
    Context(Reply<(Client, Stats)>),
    Refresh(Option<Reply<()>>),
    Event(u64, Event),
}

pub enum Event {
    Key(KeyData),
    Models(Vec<ModelCost>),
    Today(String, Vec<ModelUsage>),
    Failed(String),
    Increase(
        chrono::NaiveDate,
        Result<crate::increase::Comparison, String>,
    ),
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
        Event::Increase(..) => {}
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
        let history_path = settings_path
            .as_ref()
            .map(|path| path.with_file_name("history.json"));
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
                    let notify: Notify = Arc::new(move |cfg, request| {
                        match request {
                            NotificationRequest::Budget(key) => notifier.check(
                                &app,
                                &cfg,
                                key.period_spend,
                                key.max_budget,
                                key.budget_reset_at.as_deref(),
                            ),
                            NotificationRequest::IncreaseDue(day) => {
                                return notifier.increase_due(&cfg, day)
                            }
                            NotificationRequest::Increase(day, comparison) => {
                                notifier.check_increase(&app, &cfg, day, comparison)?
                            }
                        }
                        Ok(false)
                    });
                    coordinate(
                        store,
                        StoragePaths {
                            stats: cache,
                            history: history_path,
                        },
                        notify,
                        publish,
                        commands,
                        receiver,
                        snapshots,
                    )
                    .await
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
    pub async fn history(&self, mode: HistoryMode) -> Result<Detail<HistoryDetails>, String> {
        self.request(|reply| Command::History(mode, reply)).await
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

#[derive(Default)]
struct StoragePaths {
    stats: Option<PathBuf>,
    history: Option<PathBuf>,
}

async fn coordinate(
    store: Arc<SettingsStore>,
    paths: StoragePaths,
    notify: Notify,
    publish: impl Fn(&Stats) + Send + 'static,
    commands: mpsc::UnboundedSender<Command>,
    mut receiver: mpsc::UnboundedReceiver<Command>,
    snapshots: watch::Sender<Stats>,
) {
    let http = Client::http();
    let mut client = connection(&store, &http);
    let source = store.get().source_id;
    let cache = paths.stats;
    let history_path = paths.history;
    let load_path = history_path.clone();
    let history_source = source.clone();
    let mut history = tauri::async_runtime::spawn_blocking(move || {
        crate::history::History::load(
            load_path.as_deref(),
            history_source,
            chrono::Utc::now().date_naive(),
        )
    })
    .await
    .unwrap_or_else(|_| {
        crate::history::History::empty(source.clone(), chrono::Utc::now().date_naive())
    });
    let mut history_worker: Option<(chrono::NaiveDate, tauri::async_runtime::JoinHandle<()>)> =
        None;
    let mut history_waiters: Vec<Reply<Detail<HistoryDetails>>> = Vec::new();
    let mut history_force_followup = false;
    let mut history_error: Option<String> = None;
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
    let mut increase_retry = IncreaseRetry::default();
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
                    let cfg = store.get();
                    let day = chrono::Utc::now().date_naive();
                    let check_increase = if cfg.spend_increase.enabled
                        && increase_retry.ready(tokio::time::Instant::now())
                    {
                        let notify = notify.clone();
                        match tauri::async_runtime::spawn_blocking(move || {
                            notify(cfg, NotificationRequest::IncreaseDue(day))
                        })
                        .await
                        {
                            Ok(Ok(due)) => due,
                            result => {
                                eprintln!("Détection de hausse indisponible : {result:?}");
                                increase_retry.failed(tokio::time::Instant::now());
                                false
                            }
                        }
                    } else {
                        false
                    };
                    let commands = commands.clone();
                    let cycle = stats.cycle_id;
                    worker = Some(tauri::async_runtime::spawn(async move {
                        run_cycle(&client, |event| {
                            if !matches!(event, Event::Finished) {
                                let _ = commands.send(Command::Event(cycle, event));
                            }
                        })
                        .await;
                        if check_increase {
                            let (reply, response) = oneshot::channel();
                            let result = if commands
                                .send(Command::History(HistoryMode::Refresh, reply))
                                .is_ok()
                            {
                                response
                                    .await
                                    .map_err(|_| Error::Closed.to_string())
                                    .and_then(|r| r)
                                    .and_then(|detail| {
                                        crate::history::comparison(day, &detail.data)
                                    })
                            } else {
                                Err(Error::Closed.to_string())
                            };
                            let _ =
                                commands.send(Command::Event(cycle, Event::Increase(day, result)));
                        }
                        let _ = commands.send(Command::Event(cycle, Event::Finished));
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
            Command::History(mode, reply) => {
                let now = chrono::Utc::now();
                history.roll(now.date_naive());
                if mode == HistoryMode::Cached {
                    let _ = reply.send(Ok(Detail {
                        generation: stats.generation,
                        data: history.view(history_error.clone()),
                    }));
                    continue;
                }
                if let Some((start, _)) = &history_worker {
                    history_force_followup |=
                        mode == HistoryMode::Force && *start == now.date_naive();
                    history_waiters.push(reply);
                    continue;
                }
                if let Some(start) = history.range(now, mode) {
                    match client.clone() {
                        Ok(client) => {
                            history_waiters.push(reply);
                            let commands = commands.clone();
                            let generation = stats.generation;
                            history_worker = Some((
                                start,
                                tauri::async_runtime::spawn(async move {
                                    let result = client
                                        .fetch_history(now.date_naive(), start)
                                        .await
                                        .map(Box::new);
                                    let _ = commands.send(Command::HistoryFetched(
                                        generation, now, start, result,
                                    ));
                                }),
                            ));
                        }
                        Err(error) => {
                            let _ = reply.send(Ok(Detail {
                                generation: stats.generation,
                                data: history.view(Some(error)),
                            }));
                        }
                    }
                } else {
                    let _ = reply.send(Ok(Detail {
                        generation: stats.generation,
                        data: history.view(history_error.clone()),
                    }));
                }
            }
            Command::HistoryFetched(generation, now, start, result) => {
                if generation != stats.generation {
                    continue;
                }
                history_worker = None;
                match result {
                    Ok(amounts) => {
                        let previous = history.clone();
                        history.merge(now, start, *amounts);
                        history_error = None;
                        if history != previous {
                            if let Some(path) = history_path.clone() {
                                let snapshot = history.clone();
                                history_error = tauri::async_runtime::spawn_blocking(move || {
                                    crate::persistence::write(&path, &snapshot)
                                })
                                .await
                                .map_err(|_| "Sauvegarde de l'historique interrompue".to_string())
                                .and_then(|r| r.map_err(|e| e.to_string()))
                                .err();
                            }
                        }
                    }
                    Err(error) => history_error = Some(error),
                }
                history.roll(chrono::Utc::now().date_naive());
                let followup = std::mem::take(&mut history_force_followup);
                for reply in history_waiters.drain(..) {
                    if followup {
                        let _ = commands.send(Command::History(HistoryMode::Force, reply));
                    } else {
                        let _ = reply.send(Ok(Detail {
                            generation: stats.generation,
                            data: history.view(history_error.clone()),
                        }));
                    }
                }
            }
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
                if let Some((_, worker)) = history_worker.take() {
                    worker.abort();
                }
                for waiter in history_waiters.drain(..) {
                    let _ = waiter.send(Err(Error::Obsolete.to_string()));
                }
                history_force_followup = false;
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
                let previous = store.get();
                let saved_store = store.clone();
                let saved = tauri::async_runtime::spawn_blocking(move || saved_store.save(input))
                    .await
                    .map_err(|_| "La sauvegarde a été interrompue".to_string())
                    .and_then(|r| r);
                if saved.is_ok()
                    && (previous.source_id != store.get().source_id
                        || previous.spend_increase != store.get().spend_increase)
                {
                    increase_retry = IncreaseRetry::default();
                }
                if matches!(saved, Ok(true)) {
                    history = crate::history::History::empty(
                        store.get().source_id,
                        chrono::Utc::now().date_naive(),
                    );
                    history_error = None;
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
                    let _ = tauri::async_runtime::spawn_blocking(move || {
                        notify(cfg, NotificationRequest::Budget(key))
                    })
                    .await;
                }
                if let Event::Increase(day, result) = &event {
                    let result = match result {
                        Ok(comparison) => {
                            let notify = notify.clone();
                            let cfg = store.get();
                            let (day, comparison) = (*day, *comparison);
                            tauri::async_runtime::spawn_blocking(move || {
                                notify(cfg, NotificationRequest::Increase(day, comparison))
                            })
                            .await
                            .map_err(|_| "Analyse hebdomadaire interrompue".to_string())
                            .and_then(|r| r)
                            .map(|_| ())
                        }
                        Err(error) => Err(error.clone()),
                    };
                    if let Err(error) = result {
                        increase_retry.failed(tokio::time::Instant::now());
                        stats.error = Some(format!("Détection de hausse : {error}"));
                    } else {
                        increase_retry = IncreaseRetry::default();
                    }
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
    if let Some((_, worker)) = history_worker {
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
        start_with_notify(
            store,
            cache,
            Arc::new(move |_, request| {
                if matches!(request, NotificationRequest::Budget(_)) {
                    calls.fetch_add(1, Ordering::SeqCst);
                }
                Ok(false)
            }),
        )
    }

    fn start_with_notify(
        store: Arc<SettingsStore>,
        cache: Option<PathBuf>,
        notify: Notify,
    ) -> (Service, tokio::task::JoinHandle<()>) {
        start_with_storage(
            store,
            StoragePaths {
                stats: cache,
                ..Default::default()
            },
            notify,
        )
    }

    fn start_with_storage(
        store: Arc<SettingsStore>,
        paths: StoragePaths,
        notify: Notify,
    ) -> (Service, tokio::task::JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (snapshots, updates) = watch::channel(Stats::default());
        let service = Service {
            commands: commands.clone(),
            snapshots: updates,
        };
        let task = tokio::spawn(coordinate(
            store,
            paths,
            notify,
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
    async fn history_survives_restart_and_disk_failure_keeps_the_in_memory_snapshot() {
        let server = MockServer::start();
        let today = chrono::Utc::now().date_naive();
        let first = (today - chrono::Duration::days(29)).to_string();
        server.mock(|when, then| {
            when.path("/spend/logs").query_param("start_date", first);
            then.status(200)
                .json_body(serde_json::json!([{ "startTime":today.to_string(), "spend":7.0 }]));
        });
        mocks(&server, 1.0, 0);
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let path = std::env::temp_dir().join(format!("history-{}.json", uuid::Uuid::new_v4()));
        let paths = || StoragePaths {
            stats: None,
            history: Some(path.clone()),
        };
        let (service, task) =
            start_with_storage(store.clone(), paths(), Arc::new(|_, _| Ok(false)));
        let fresh = service.history(HistoryMode::Refresh).await.unwrap().data;
        assert!(fresh.error.is_none());
        assert!(path.is_file());
        task.abort();
        let (service, task) =
            start_with_storage(store.clone(), paths(), Arc::new(|_, _| Ok(false)));
        let cached = service.history(HistoryMode::Cached).await.unwrap().data;
        assert_eq!(cached, fresh);
        task.abort();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let (service, task) = start_with_storage(store, paths(), Arc::new(|_, _| Ok(false)));
        let unavailable = service.history(HistoryMode::Refresh).await.unwrap().data;
        assert_eq!(unavailable.days[29].spend, Some(7.0));
        assert!(unavailable.error.is_some());
        assert_eq!(
            service
                .history(HistoryMode::Cached)
                .await
                .unwrap()
                .data
                .days,
            unavailable.days
        );
        task.abort();
        std::fs::remove_dir(path).unwrap();
    }

    #[tokio::test]
    async fn calendar_shares_inflight_requests_preserves_cache_on_failure_and_forces_corrections() {
        let server = MockServer::start();
        let today = chrono::Utc::now().date_naive();
        let first = (today - chrono::Duration::days(29)).to_string();
        let mut full = server.mock(|when, then| {
            when.path("/spend/logs")
                .query_param("start_date", first.clone());
            then.status(200)
                .delay(Duration::from_millis(30))
                .json_body(serde_json::json!([
                    {"startTime":today.to_string(),"spend":5.0}
                ]));
        });
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(200)
                .json_body(serde_json::json!({"info":{"spend":1.0}}));
        });
        let mut fallback = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(serde_json::json!([]));
        });
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let (service, task) = start(store, Arc::new(AtomicUsize::new(0)), None);
        finished(&service, 1).await;
        assert!(service
            .history(HistoryMode::Cached)
            .await
            .unwrap()
            .data
            .days
            .iter()
            .all(|d| d.spend.is_none()));
        full.assert_hits(0);
        let (a, b) = tokio::join!(
            service.history(HistoryMode::Refresh),
            service.history(HistoryMode::Refresh)
        );
        assert_eq!(a.unwrap().data.days[29].spend, Some(5.0));
        assert_eq!(b.unwrap().data.days[29].spend, Some(5.0));
        service.history(HistoryMode::Refresh).await.unwrap();
        full.assert_hits(1);
        full.delete();
        fallback.delete();
        let mut failed = server.mock(|when, then| {
            when.path("/spend/logs")
                .query_param("start_date", first.clone());
            then.status(503);
        });
        let stale = service.history(HistoryMode::Force).await.unwrap().data;
        assert_eq!(stale.days[29].spend, Some(5.0));
        assert!(stale.error.unwrap().contains("503"));
        failed.assert_hits(1);
        failed.delete();
        let corrected = server.mock(|when, then| {
            when.path("/spend/logs").query_param("start_date", first);
            then.status(200).json_body(serde_json::json!([
                {"startTime":today.to_string(),"spend":2.0}
            ]));
        });
        let result = service.history(HistoryMode::Force).await.unwrap().data;
        assert_eq!(result.days[29].spend, Some(2.0));
        assert!(result.error.is_none());
        corrected.assert_hits(1);
        task.abort();
    }

    #[tokio::test]
    async fn saving_a_connection_cancels_history_waiters_and_rejects_old_results() {
        let old = MockServer::start();
        let new = MockServer::start();
        mocks(&old, 1.0, 100);
        mocks(&new, 2.0, 0);
        let store = Arc::new(SettingsStore::for_test(&old.base_url()));
        store.save(preferences(&old, false)).unwrap();
        let (service, task) = start(store, Arc::new(AtomicUsize::new(0)), None);
        let previous = finished(&service, 1).await;
        let (reply, response) = oneshot::channel();
        service
            .commands
            .send(Command::History(HistoryMode::Refresh, reply))
            .unwrap();
        service.save(preferences(&new, false)).await.unwrap();
        assert!(response.await.unwrap().is_err());
        let now = chrono::Utc::now();
        service
            .commands
            .send(Command::HistoryFetched(
                previous.generation,
                now,
                now.date_naive() - chrono::Duration::days(29),
                Ok(Box::new([Some(999.0); 30])),
            ))
            .unwrap();
        let snapshot = service.history(HistoryMode::Cached).await.unwrap();
        assert!(snapshot.data.days.iter().all(|d| d.spend.is_none()));
        task.abort();
    }

    #[test]
    fn increase_retry_waits_fifteen_minutes_without_sleeping() {
        let now = tokio::time::Instant::now();
        let mut retry = IncreaseRetry::default();
        assert!(retry.ready(now));
        retry.failed(now);
        assert!(!retry.ready(now + Duration::from_secs(899)));
        assert!(retry.ready(now + Duration::from_secs(900)));
    }

    fn preferences(server: &MockServer, enabled: bool) -> SettingsInput {
        SettingsInput {
            base_url: server.base_url(),
            refresh_secs: 30,
            notifications_enabled: true,
            notify_info_percent: 50,
            notify_critical_percent: 75,
            spend_increase: shared::SpendIncreaseSettings {
                enabled,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn history_is_disabled_or_checked_once_and_obsolete_events_are_ignored() {
        let server = MockServer::start();
        let date = chrono::Utc::now().date_naive();
        let historical_start = (date - chrono::Duration::days(29)).to_string();
        let history = server.mock(|when, then| {
            when.path("/spend/logs")
                .query_param("start_date", historical_start);
            then.status(200).json_body(serde_json::json!([
                {"startTime": (date - chrono::Duration::days(1)).to_string(), "spend": 10.0}
            ]));
        });
        mocks(&server, 1.0, 0);
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let state = Arc::new(std::sync::Mutex::new(crate::increase::State::default()));
        let sent = Arc::new(AtomicUsize::new(0));
        let state_for_notify = state.clone();
        let sent_for_notify = sent.clone();
        let (service, task) = start_with_notify(
            store,
            None,
            Arc::new(move |cfg, request| {
                let mut state = state_for_notify.lock().unwrap();
                match request {
                    NotificationRequest::Budget(_) => {}
                    NotificationRequest::IncreaseDue(day) => {
                        return Ok(state.due(day, cfg.spend_increase))
                    }
                    NotificationRequest::Increase(day, comparison) => {
                        let (next, send) = state.evaluate(day, cfg.spend_increase, comparison);
                        *state = next;
                        if send {
                            sent_for_notify.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
                Ok(false)
            }),
        );
        let disabled = finished(&service, 1).await;
        history.assert_hits(0);
        service.save(preferences(&server, true)).await.unwrap();
        let active = finished(&service, disabled.cycle_id + 2).await;
        history.assert_hits(1);
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        service.refresh_and_wait().await.unwrap();
        let repeated = finished(&service, active.cycle_id + 1).await;
        history.assert_hits(1);
        // Un résultat ancien ne doit pas réarmer l'épisode en cours.
        service
            .commands
            .send(Command::Event(
                disabled.cycle_id,
                Event::Increase(
                    date,
                    Ok(crate::increase::Comparison {
                        recent: 0.0,
                        reference: 0.0,
                    }),
                ),
            ))
            .unwrap();
        service.refresh_and_wait().await.unwrap();
        finished(&service, repeated.cycle_id + 1).await;
        assert!(state.lock().unwrap().fired);
        let mut changed = preferences(&server, true);
        changed.spend_increase.min_usd = 4.0;
        let previous_cycle = service.stats().cycle_id;
        service.save(changed).await.unwrap();
        finished(&service, previous_cycle + 2).await;
        history.assert_hits(1); // Un nouveau seuil réutilise l'historique déjà synchronisé.
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn failed_history_preserves_budget_updates_and_manual_refresh_respects_backoff() {
        let server = MockServer::start();
        let historical_start =
            (chrono::Utc::now().date_naive() - chrono::Duration::days(29)).to_string();
        let history = server.mock(|when, then| {
            when.path("/spend/logs")
                .query_param("start_date", historical_start);
            then.status(503);
        });
        mocks(&server, 42.0, 0);
        let budget_calls = Arc::new(AtomicUsize::new(0));
        let calls = budget_calls.clone();
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        let (service, task) = start_with_notify(
            store,
            None,
            Arc::new(move |_, request| {
                match request {
                    NotificationRequest::Budget(_) => {
                        calls.fetch_add(1, Ordering::SeqCst);
                    }
                    NotificationRequest::IncreaseDue(_) => return Ok(true),
                    NotificationRequest::Increase(..) => {
                        panic!("Une erreur réseau ne doit pas modifier l'épisode")
                    }
                }
                Ok(false)
            }),
        );
        let failed = finished(&service, 1).await;
        assert_eq!(failed.period_spend, 42.0);
        assert!(failed.error.unwrap().contains("503"));
        history.assert_hits(1);
        service.refresh_and_wait().await.unwrap();
        finished(&service, failed.cycle_id + 1).await;
        history.assert_hits(1);
        assert_eq!(budget_calls.load(Ordering::SeqCst), 2);
        task.abort();
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

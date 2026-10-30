//! Coordinateur : seul propriétaire de l'état publié. Les résultats portent un cycle,
//! et les sauvegardes interrompent ce cycle avant de changer la configuration.
use crate::{
    litellm::{Client, KeyData},
    notify::Notifier,
    settings::{Settings, SettingsStore},
};
use chrono::Local;
#[cfg(test)]
use shared::ModelUsage;
use shared::{
    Activity, Detail, HistoryDetails, HistoryMode, ModelCost, SettingsInput, SettingsView, Stats,
    WeekDetails,
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

pub(crate) enum NotificationRequest {
    Budget(KeyData),
    IncreaseDue(chrono::NaiveDate),
    Increase(chrono::NaiveDate, crate::increase::Comparison),
}

pub(crate) type Notify =
    Arc<dyn Fn(Settings, NotificationRequest) -> Result<bool, String> + Send + Sync>;

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
    BeginDailyRefresh(String, Reply<Detail<u64>>),
    DailyContext(
        String,
        Option<u64>,
        Reply<(Client, Stats, u64, Option<std::time::Instant>)>,
    ),
    CachedDaily(String, Reply<Detail<Option<shared::CachedDaily>>>),
    StoreDaily(
        u64,
        u64,
        crate::litellm::calendar::DayBounds,
        Option<shared::DailyCosts>,
        Option<shared::DailyUsage>,
        chrono::DateTime<chrono::Utc>,
        Reply<()>,
    ),
    LocalHistory(HistoryMode, Reply<shared::LocalHistorySnapshot>),
    LocalProgress(u64, u64, shared::HistoryDay),
    LocalFinished(
        u64,
        u64,
        chrono::DateTime<chrono::Utc>,
        chrono::NaiveDate,
        Result<(), String>,
    ),
    CalendarTick,
    /// Barrière de test : répond une fois les écritures déjà demandées terminées.
    #[cfg(test)]
    Flush(oneshot::Sender<()>),
    HarnessSettings(Reply<shared::HarnessSettings>),
    SaveHarnessSettings(shared::HarnessSettings, Reply<shared::HarnessSettings>),
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
    Refresh(Option<u64>, Option<Reply<()>>),
    Event(u64, Event),
}

pub enum Event {
    Key(KeyData),
    Models(Vec<ModelCost>),
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
            stats.whole_proxy = key.whole_proxy;
            stats.period_spend = key.period_spend;
            stats.max_budget = key.max_budget;
            stats.budget_reset_at = key.budget_reset_at;
            stats.period_start = key.period_start;
        }
        Event::Models(models) => stats.models_total = models,
        Event::Failed(error) => stats.error = Some(error),
        Event::Finished => stats.refreshing = false,
        Event::Increase(..) => {}
    }
}

async fn run_cycle_with_today(
    client: &Client,
    today: impl std::future::Future<Output = Result<Option<Event>, String>>,
    publish: impl Fn(Event) + Sync,
) {
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
        match today.await {
            Ok(Some(event)) => publish(event),
            Ok(None) => {}
            Err(error) => publish(Event::Failed(error)),
        }
    };
    tokio::join!(models, today);
    publish(Event::Finished);
}

#[derive(Clone)]
pub struct Service {
    commands: mpsc::UnboundedSender<Command>,
    snapshots: watch::Receiver<Stats>,
    local_snapshots: watch::Receiver<shared::LocalHistorySnapshot>,
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
        let (local_snapshots, local_updates) =
            watch::channel(shared::LocalHistorySnapshot::default());
        let service = Self {
            commands: commands.clone(),
            snapshots: updates,
            local_snapshots: local_updates,
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
                        (snapshots, local_snapshots),
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
        let _ = self.commands.send(Command::Refresh(None, None));
    }
    #[cfg(test)]
    pub async fn refresh_and_wait(&self) -> Result<(), String> {
        self.refresh_with_day(None).await
    }
    pub async fn refresh_with_day(&self, today_refresh: Option<u64>) -> Result<(), String> {
        self.request(|reply| Command::Refresh(today_refresh, Some(reply)))
            .await
    }
    async fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| Error::Closed.to_string())?;
        result.await.map_err(|_| Error::Closed.to_string())?
    }
    pub async fn harness_settings(&self) -> Result<shared::HarnessSettings, String> {
        self.request(Command::HarnessSettings).await
    }
    pub async fn save_harness_settings(
        &self,
        input: shared::HarnessSettings,
    ) -> Result<shared::HarnessSettings, String> {
        self.request(|r| Command::SaveHarnessSettings(input, r))
            .await
    }
    pub async fn harness_prices(&self) -> Result<(u64, serde_json::Value), String> {
        let (client, state) = self.request(Command::Context).await?;
        let value = client.fetch_model_prices().await?;
        if self.stats().generation != state.generation {
            return Err(Error::Obsolete.to_string());
        }
        Ok((state.generation, value))
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
    pub async fn local_history(
        &self,
        mode: HistoryMode,
    ) -> Result<shared::LocalHistorySnapshot, String> {
        self.request(|reply| Command::LocalHistory(mode, reply))
            .await
    }
    #[cfg(test)]
    async fn flush(&self) {
        let (reply, done) = oneshot::channel();
        let _ = self.commands.send(Command::Flush(reply));
        let _ = done.await;
    }
    pub fn subscribe_local_history(&self) -> watch::Receiver<shared::LocalHistorySnapshot> {
        self.local_snapshots.clone()
    }
    pub async fn cached_day(
        &self,
        date: String,
    ) -> Result<Detail<Option<shared::CachedDaily>>, String> {
        self.request(|reply| Command::CachedDaily(date, reply))
            .await
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
    #[cfg(test)]
    pub async fn day_usage(
        &self,
        date: String,
        force: bool,
    ) -> Result<Detail<shared::DailyUsage>, String> {
        self.day_usage_in_refresh(date, force, None).await
    }

    pub async fn begin_day_refresh(&self, date: String) -> Result<Detail<u64>, String> {
        self.request(|reply| Command::BeginDailyRefresh(date, reply))
            .await
    }

    pub async fn day_usage_in_refresh(
        &self,
        date: String,
        force: bool,
        refresh: Option<u64>,
    ) -> Result<Detail<shared::DailyUsage>, String> {
        let refresh = if force && refresh.is_none() {
            Some(self.begin_day_refresh(date.clone()).await?.data)
        } else {
            refresh
        };
        let (client, state, attempt, since) = self
            .request(|reply| Command::DailyContext(date.clone(), refresh, reply))
            .await?;
        // Un cache périmé se recharge normalement : seule une actualisation explicite force.
        let mut cached_usage = None;
        if !force {
            if let Some(cached) = self.cached_day(date.clone()).await?.data {
                if !cached.usage_stale {
                    if let Some(usage) = cached.usage {
                        if let Some(when) = cached.usage_fetched_at.and_then(|t| t.parse().ok()) {
                            cached_usage = Some(crate::litellm::Validated {
                                data: usage,
                                fetched_at: when,
                            });
                        }
                    }
                }
            }
        }
        let bounds = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
            .ok()
            .and_then(|d| crate::litellm::calendar::day_bounds(d, &Local));
        let mut updates = self.snapshots.clone();
        let data = if let Some(cached) = cached_usage {
            cached
        } else {
            tokio::select! {
                result = client.fetch_day_shared(&date, force, since) => result?,
                _ = updates.wait_for(|snapshot| snapshot.generation != state.generation) => return Err(Error::Obsolete.to_string()),
            }
        };
        let requested = data.fetched_at;
        let data = data.data;
        validate_daily_bounds(
            bounds,
            chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .ok()
                .and_then(|d| crate::litellm::calendar::day_bounds(d, &Local)),
        )?;
        if let Some(bounds) = bounds {
            self.request(|reply| {
                Command::StoreDaily(
                    state.generation,
                    attempt,
                    bounds,
                    None,
                    Some(data.clone()),
                    requested,
                    reply,
                )
            })
            .await?;
        }
        self.validated(state.generation, data, Some(requested))
    }
    #[cfg(test)]
    pub async fn day_costs(
        &self,
        date: String,
        force: bool,
    ) -> Result<Detail<shared::DailyCosts>, String> {
        self.day_costs_in_refresh(date, force, None).await
    }

    pub async fn day_costs_in_refresh(
        &self,
        date: String,
        force: bool,
        refresh: Option<u64>,
    ) -> Result<Detail<shared::DailyCosts>, String> {
        let refresh = if force && refresh.is_none() {
            Some(self.begin_day_refresh(date.clone()).await?.data)
        } else {
            refresh
        };
        let (client, state, attempt, since) = self
            .request(|reply| Command::DailyContext(date.clone(), refresh, reply))
            .await?;
        if !force {
            if let Some(cached) = self
                .cached_day(date.clone())
                .await?
                .data
                .filter(|c| !c.stale)
            {
                let validated_at = cached
                    .fetched_at
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .ok();
                return self.validated(state.generation, cached.costs, validated_at);
            }
        }
        let bounds = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
            .ok()
            .and_then(|d| crate::litellm::calendar::day_bounds(d, &Local));
        let mut updates = self.snapshots.clone();
        let data = tokio::select! {
            result=client.fetch_day_costs_validated(&date,force,&Local,since)=>result?,
            _=updates.wait_for(|snapshot| snapshot.generation != state.generation)=>return Err(Error::Obsolete.to_string()),
        };
        let requested = data.fetched_at;
        let data = data.data;
        validate_daily_bounds(
            bounds,
            chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .ok()
                .and_then(|d| crate::litellm::calendar::day_bounds(d, &Local)),
        )?;
        if let Some(bounds) = bounds {
            self.request(|reply| {
                Command::StoreDaily(
                    state.generation,
                    attempt,
                    bounds,
                    Some(data.clone()),
                    None,
                    requested,
                    reply,
                )
            })
            .await?;
        }
        self.validated(state.generation, data, Some(requested))
    }
    fn validated<T>(
        &self,
        generation: u64,
        data: T,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Detail<T>, String> {
        let mut detail = self.detail(generation, data)?;
        detail.validated_at = at.map(|t| t.timestamp_micros());
        Ok(detail)
    }
    fn detail<T>(&self, generation: u64, data: T) -> Result<Detail<T>, String> {
        if generation != self.snapshots.borrow().generation {
            return Err(Error::Obsolete.to_string());
        }
        Ok(Detail {
            generation,
            data,
            validated_at: None,
        })
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

fn validate_daily_bounds(
    requested: Option<crate::litellm::calendar::DayBounds>,
    current: Option<crate::litellm::calendar::DayBounds>,
) -> Result<(), String> {
    if requested != current {
        Err("Le fuseau horaire a changé ; rechargez cette journée".into())
    } else {
        Ok(())
    }
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
    publications: (
        watch::Sender<Stats>,
        watch::Sender<shared::LocalHistorySnapshot>,
    ),
) {
    let (snapshots, local_snapshots) = publications;
    let http = Client::http();
    let mut client = connection(&store, &http);
    let source = store.get().source_id;
    let cache = paths.stats;
    let history_path = paths.history;
    let daily_path = history_path
        .as_ref()
        .map(|p| p.with_file_name("daily-usage.json"));
    let load_daily = daily_path.clone();
    let daily_source = source.clone();
    let mut daily_store = tauri::async_runtime::spawn_blocking(move || {
        crate::daily_store::DailyStore::load(load_daily.as_deref(), daily_source)
    })
    .await
    .unwrap_or_else(|_| crate::daily_store::DailyStore::load(None, source.clone()));
    let daily_writer = daily_path
        .clone()
        .map(crate::persistence::Writer::<crate::daily_store::DailyStore>::spawn);
    let mut today_validated: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut usage_revision = 0u64;
    let mut usage_revisions: Vec<(String, u64)> = Vec::new();
    let mut daily_sequence = 0u64;
    let mut daily_attempts = std::collections::VecDeque::<(String, u64, std::time::Instant)>::new();
    let local_path = history_path
        .as_ref()
        .map(|p| p.with_file_name("history-local.json"));
    let mut calendar = crate::local_history::LocalHistory::load(
        local_path.as_deref(),
        source.clone(),
        crate::litellm::calendar::CalendarWindow::new(Local::now().date_naive(), &Local),
    );
    let calendar_writer = local_path
        .clone()
        .map(crate::persistence::Writer::<crate::local_history::LocalHistory>::spawn);
    let stats_writer = cache
        .clone()
        .map(crate::persistence::Writer::<Stats>::spawn);
    let mut local_worker: Option<tauri::async_runtime::JoinHandle<()>> = None;
    let mut local_run = 0u64;
    // Le changement de jour ou de fuseau n'exige pas une précision à la seconde.
    let mut local_tick = tokio::time::interval(Duration::from_secs(30));
    local_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
    // Les deux caches s'écrivent indépendamment : des coûts du jour corrigés peuvent
    // être persistés sans les statistiques qui suivent.
    if let Some(bounds) = crate::litellm::calendar::day_bounds(Local::now().date_naive(), &Local) {
        drop_contradicted_today(&mut stats, &daily_store, bounds, None);
    }
    let mut worker: Option<tauri::async_runtime::JoinHandle<()>> = None;
    let mut preloaded_day = None;
    let mut preload_worker: Option<tauri::async_runtime::JoinHandle<()>> = None;
    let mut increase_retry = IncreaseRetry::default();
    let mut pending = true;
    let mut force_cycle = false;
    let mut cycle_refresh = None;
    let mut next = tokio::time::Instant::now();
    publish_calendar(
        &local_snapshots,
        &calendar,
        &usage_revisions,
        stats.generation,
        false,
    );
    loop {
        if pending && worker.is_none() {
            pending = false;
            let force = std::mem::take(&mut force_cycle);
            let refresh = cycle_refresh.take();
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
                    let service = Service {
                        commands: commands.clone(),
                        snapshots: snapshots.subscribe(),
                        local_snapshots: local_snapshots.subscribe(),
                    };
                    worker = Some(tauri::async_runtime::spawn(async move {
                        let today = async {
                            let result = service
                                .day_usage_in_refresh(
                                    Local::now().date_naive().to_string(),
                                    force,
                                    refresh,
                                )
                                .await;
                            match result {
                                Ok(_) => Ok(None),
                                Err(error)
                                    if error == Error::Obsolete.to_string()
                                        || error == crate::litellm::DAY_SUPERSEDED =>
                                {
                                    Ok(None)
                                }
                                Err(error) => Err(error),
                            }
                        };
                        run_cycle_with_today(&client, today, |event| {
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
            _ = local_tick.tick() => Command::CalendarTick,
            command = receiver.recv() => match command { Some(c) => c, None => break },
            _ = tokio::time::sleep_until(next), if worker.is_none() => {
                let mut refreshed = connection(&store, &http);
                if let (Ok(next), Ok(previous)) = (&mut refreshed, &client) {
                    next.reuse_daily_cache(previous);
                }
                client = refreshed;
                pending = true;
                continue;
            }
        };
        match command {
            Command::BeginDailyRefresh(date, reply) => {
                let valid = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                    .ok()
                    .is_some_and(|d| d.to_string() == date && d <= Local::now().date_naive());
                if !valid {
                    let _ = reply.send(Err("Date invalide ou future".into()));
                    continue;
                }
                daily_sequence += 1;
                daily_attempts.retain(|(d, _, _)| d != &date);
                daily_attempts.push_back((date, daily_sequence, std::time::Instant::now()));
                while daily_attempts.len() > 30 {
                    daily_attempts.pop_front();
                }
                let _ = reply.send(Ok(Detail {
                    generation: stats.generation,
                    data: daily_sequence,
                    validated_at: None,
                }));
            }
            Command::DailyContext(date, refresh, reply) => {
                if refresh.is_none() && !daily_attempts.iter().any(|(d, _, _)| d == &date) {
                    daily_sequence += 1;
                    daily_attempts.push_back((
                        date.clone(),
                        daily_sequence,
                        std::time::Instant::now(),
                    ));
                    while daily_attempts.len() > 30 {
                        daily_attempts.pop_front();
                    }
                }
                let current = daily_attempts.iter().find(|(d, _, _)| d == &date);
                let value = match current {
                    Some((_, id, started)) if refresh.is_none_or(|r| r == *id) => client
                        .clone()
                        .map(|c| (c, stats.clone(), *id, refresh.map(|_| *started))),
                    _ => Err(Error::Obsolete.to_string()),
                };
                let _ = reply.send(value);
            }
            Command::CachedDaily(date, reply) => {
                let value = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                    .ok()
                    .filter(|d| d.to_string() == date)
                    .and_then(|d| crate::litellm::calendar::day_bounds(d, &Local))
                    .and_then(|bounds| daily_store.get(bounds, chrono::Utc::now()));
                // Un aperçu garde l'instant de validation de ses coûts pour rester ordonné.
                let validated_at = value
                    .as_ref()
                    .and_then(|c| c.fetched_at.parse::<chrono::DateTime<chrono::Utc>>().ok())
                    .map(|t| t.timestamp_micros());
                let _ = reply.send(Ok(Detail {
                    generation: stats.generation,
                    data: value,
                    validated_at,
                }));
            }
            Command::StoreDaily(generation, attempt, bounds, costs, usage, when, reply) => {
                if generation != stats.generation
                    || Some(bounds) != crate::litellm::calendar::day_bounds(bounds.date, &Local)
                    || !daily_attempts
                        .iter()
                        .any(|(date, id, _)| date == &bounds.date.to_string() && *id == attempt)
                {
                    let _ = reply.send(Err(Error::Obsolete.to_string()));
                    continue;
                }
                if let Some(value) = &usage {
                    if daily_store
                        .get(bounds, chrono::Utc::now())
                        .and_then(|c| c.usage_fetched_at)
                        .and_then(|t| t.parse::<chrono::DateTime<chrono::Utc>>().ok())
                        .is_some_and(|t| t > when)
                    {
                        let _ = reply.send(Err(Error::Obsolete.to_string()));
                        continue;
                    }
                    let contradicted = daily_store
                        .get(bounds, chrono::Utc::now())
                        .filter(|c| {
                            c.fetched_at
                                .parse::<chrono::DateTime<chrono::Utc>>()
                                .is_ok_and(|t| t > when)
                        })
                        .is_some_and(|c| contradicts(&value.models, &c.costs));
                    if bounds.date == Local::now().date_naive() && !contradicted {
                        today_validated = Some(when);
                        stats.today_date = Some(value.date.clone());
                        stats.today = value.models.clone();
                        stats.updated_at = Some(Local::now().format("%H:%M:%S").to_string());
                        stats.revision += 1;
                        snapshots.send_replace(stats.clone());
                        publish(&stats);
                    }
                }
                let spend = usage
                    .as_ref()
                    .map(|u| u.models.iter().map(|m| m.spend).sum())
                    .or_else(|| {
                        costs
                            .as_ref()
                            .map(|c| c.models.iter().map(|m| m.spend).sum())
                    });
                let has_usage = usage.is_some();
                let stored = daily_store.put(bounds, costs, usage, when);
                if drop_contradicted_today(&mut stats, &daily_store, bounds, today_validated) {
                    stats.revision += 1;
                    snapshots.send_replace(stats.clone());
                    publish(&stats);
                    // Seul un aperçu de coûts relance un cycle : un cycle rejeté ne boucle pas.
                    pending |= !has_usage;
                }
                if stored {
                    if let Some(writer) = &daily_writer {
                        writer.save(daily_store.clone());
                    }
                }
                // De nouveaux tokens à coût inchangé doivent aussi rafraîchir un récapitulatif ouvert.
                let usage_changed = stored && has_usage && {
                    usage_revision += 1;
                    let date = bounds.date.to_string();
                    usage_revisions.retain(|(d, _)| d != &date);
                    usage_revisions.push((date, usage_revision));
                    while usage_revisions.len() > 30 {
                        usage_revisions.remove(0);
                    }
                    true
                };
                let spend_changed =
                    spend.is_some_and(|spend| calendar.merge_day(bounds, spend, when));
                if spend_changed {
                    persist_calendar(&calendar_writer, &calendar);
                }
                if spend_changed || usage_changed {
                    publish_calendar(
                        &local_snapshots,
                        &calendar,
                        &usage_revisions,
                        generation,
                        local_worker.is_some(),
                    );
                }
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            Command::Flush(reply) => {
                let writers = (
                    daily_writer.as_ref().map(|w| w.flushed()),
                    calendar_writer.as_ref().map(|w| w.flushed()),
                    stats_writer.as_ref().map(|w| w.flushed()),
                );
                tokio::spawn(async move {
                    if let Some(f) = writers.0 {
                        f.await;
                    }
                    if let Some(f) = writers.1 {
                        f.await;
                    }
                    if let Some(f) = writers.2 {
                        f.await;
                    }
                    let _ = reply.send(());
                });
            }
            Command::CalendarTick => {
                // Vérification bon marché : la fenêtre ne change qu'avec le jour ou son fuseau.
                if !calendar.window_outdated(Local::now().date_naive(), &Local) {
                    continue;
                }
                if calendar.rebase(
                    store.get().source_id,
                    crate::litellm::calendar::CalendarWindow::new(
                        Local::now().date_naive(),
                        &Local,
                    ),
                ) {
                    local_run += 1;
                    if let Some(worker) = local_worker.take() {
                        worker.abort();
                    }
                    publish_calendar(
                        &local_snapshots,
                        &calendar,
                        &usage_revisions,
                        stats.generation,
                        false,
                    );
                }
            }
            Command::LocalHistory(mode, reply) => {
                if calendar.rebase(
                    store.get().source_id,
                    crate::litellm::calendar::CalendarWindow::new(
                        Local::now().date_naive(),
                        &Local,
                    ),
                ) {
                    local_run += 1;
                    if let Some(worker) = local_worker.take() {
                        worker.abort();
                    }
                    publish_calendar(
                        &local_snapshots,
                        &calendar,
                        &usage_revisions,
                        stats.generation,
                        false,
                    );
                }
                let now = chrono::Utc::now();
                if mode == HistoryMode::Force {
                    local_run += 1;
                    if let Some(worker) = local_worker.take() {
                        worker.abort();
                    }
                }
                if local_worker.is_none() {
                    if let Some(start) = calendar.range(now, mode) {
                        match client.clone() {
                            Ok(client) => {
                                local_run += 1;
                                let run = local_run;
                                let generation = stats.generation;
                                let today = calendar.window.today;
                                let send = commands.clone();
                                local_worker = Some(tauri::async_runtime::spawn(async move {
                                    let progress = send.clone();
                                    let result = client
                                        .fetch_local_history(today, start, move |value| {
                                            let _ = progress.send(Command::LocalProgress(
                                                generation, run, value,
                                            ));
                                        })
                                        .await;
                                    let _ = send.send(Command::LocalFinished(
                                        generation, run, now, start, result,
                                    ));
                                }));
                                publish_calendar(
                                    &local_snapshots,
                                    &calendar,
                                    &usage_revisions,
                                    stats.generation,
                                    true,
                                );
                            }
                            Err(error) => {
                                calendar.finish(now, start, Some(error));
                                publish_calendar(
                                    &local_snapshots,
                                    &calendar,
                                    &usage_revisions,
                                    stats.generation,
                                    false,
                                );
                            }
                        }
                    }
                }
                let _ = reply.send(Ok(local_snapshots.borrow().clone()));
            }
            Command::LocalProgress(generation, run, day) => {
                if generation == stats.generation
                    && run == local_run
                    && calendar.merge_estimate(day)
                {
                    publish_calendar(
                        &local_snapshots,
                        &calendar,
                        &usage_revisions,
                        generation,
                        true,
                    );
                }
            }
            Command::LocalFinished(generation, run, now, start, result) => {
                if generation != stats.generation || run != local_run {
                    continue;
                }
                local_worker = None;
                calendar.finish(now, start, result.err());
                persist_calendar(&calendar_writer, &calendar);
                publish_calendar(
                    &local_snapshots,
                    &calendar,
                    &usage_revisions,
                    generation,
                    false,
                );
            }
            Command::History(mode, reply) => {
                let now = chrono::Utc::now();
                history.roll(now.date_naive());
                if mode == HistoryMode::Cached {
                    let _ = reply.send(Ok(Detail {
                        generation: stats.generation,
                        data: history.view(history_error.clone()),
                        validated_at: None,
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
                                validated_at: None,
                            }));
                        }
                    }
                } else {
                    let _ = reply.send(Ok(Detail {
                        generation: stats.generation,
                        data: history.view(history_error.clone()),
                        validated_at: None,
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
                            validated_at: None,
                        }));
                    }
                }
            }
            Command::HarnessSettings(reply) => {
                let _ = reply.send(Ok(store.get().harnesses));
            }
            Command::SaveHarnessSettings(input, reply) => {
                let cloned = store.clone();
                let result =
                    tauri::async_runtime::spawn_blocking(move || cloned.save_harnesses(input))
                        .await
                        .map_err(|_| "Sauvegarde interrompue".to_owned())
                        .and_then(|r| r);
                let _ = reply.send(result);
            }
            Command::Settings(reply) => {
                let _ = reply.send(Ok(store.view()));
            }
            Command::Context(reply) => {
                let _ = reply.send(client.clone().map(|c| (c, stats.clone())));
            }
            Command::Refresh(today_refresh, reply) => {
                let date = Local::now().date_naive().to_string();
                let attempt = if let Some(id) = today_refresh {
                    if !daily_attempts
                        .iter()
                        .any(|(d, token, _)| d == &date && *token == id)
                    {
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(Error::Obsolete.to_string()));
                        }
                        continue;
                    }
                    id
                } else {
                    daily_sequence += 1;
                    daily_attempts.retain(|(d, _, _)| d != &date);
                    daily_attempts.push_back((date, daily_sequence, std::time::Instant::now()));
                    while daily_attempts.len() > 30 {
                        daily_attempts.pop_front();
                    }
                    daily_sequence
                };
                cycle_refresh = Some(attempt);
                force_cycle = true;
                let mut refreshed = connection(&store, &http);
                if let (Ok(next), Ok(previous)) = (&mut refreshed, &client) {
                    next.reuse_daily_cache(previous);
                }
                client = refreshed;
                pending = true;
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
            }
            Command::Save(input, reply) => {
                cycle_refresh = None;
                force_cycle = false;
                if let Some(worker) = preload_worker.take() {
                    worker.abort();
                }
                preloaded_day = None;
                local_run += 1;
                if let Some(worker) = local_worker.take() {
                    worker.abort();
                }
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
                    daily_attempts.clear();
                    usage_revisions.clear();
                    today_validated = None;
                    daily_store = crate::daily_store::DailyStore::load(None, store.get().source_id);
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
                calendar.rebase(
                    store.get().source_id,
                    crate::litellm::calendar::CalendarWindow::new(
                        Local::now().date_naive(),
                        &Local,
                    ),
                );
                publish_calendar(
                    &local_snapshots,
                    &calendar,
                    &usage_revisions,
                    stats.generation,
                    false,
                );
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
                        whole_proxy: key.whole_proxy,
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
                let finished = matches!(event, Event::Finished);
                apply(&mut stats, event);
                stats.revision += 1;
                snapshots.send_replace(stats.clone());
                publish(&stats);
                if finished {
                    worker = None;
                    let day = Local::now().date_naive();
                    if preloaded_day != Some((stats.generation, day))
                        && stats.today_date.as_deref() == Some(day.to_string().as_str())
                    {
                        preloaded_day = Some((stats.generation, day));
                        if let Some(worker) = preload_worker.take() {
                            worker.abort();
                        }
                        if let Some(yesterday) = day.pred_opt() {
                            let service = Service {
                                commands: commands.clone(),
                                snapshots: snapshots.subscribe(),
                                local_snapshots: local_snapshots.subscribe(),
                            };
                            preload_worker = Some(tauri::async_runtime::spawn(async move {
                                let _ = service
                                    .day_usage_in_refresh(yesterday.to_string(), false, None)
                                    .await;
                            }));
                        }
                    }
                    next = tokio::time::Instant::now()
                        + Duration::from_secs(store.get().refresh_secs.clamp(5, 3600).into());
                    if let Some(writer) = &stats_writer {
                        writer.save(stats.clone());
                    }
                }
            }
        }
    }
    if let Some(worker) = worker {
        worker.abort();
    }
    if let Some(worker) = preload_worker {
        worker.abort();
    }
    if let Some((_, worker)) = history_worker {
        worker.abort();
    }
    if let Some(worker) = local_worker {
        worker.abort();
    }
}

/// Des coûts plus récents qui contredisent les tokens du jour les rendent périmés,
/// y compris des tokens restaurés dont la validation est inconnue (`validated` absent) :
/// les masquer plutôt que d'afficher deux totaux différents.
fn drop_contradicted_today(
    stats: &mut Stats,
    daily_store: &crate::daily_store::DailyStore,
    bounds: crate::litellm::calendar::DayBounds,
    validated: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    if bounds.date != Local::now().date_naive()
        || stats.today_date.as_deref() != Some(bounds.date.to_string().as_str())
    {
        return false;
    }
    let newer = daily_store.get(bounds, chrono::Utc::now()).filter(|c| {
        c.fetched_at
            .parse::<chrono::DateTime<chrono::Utc>>()
            .is_ok_and(|at| validated.is_none_or(|t| at > t))
    });
    if !newer.is_some_and(|c| contradicts(&stats.today, &c.costs)) {
        return false;
    }
    stats.today.clear();
    stats.today_date = None;
    true
}

fn contradicts(usage: &[shared::ModelUsage], costs: &shared::DailyCosts) -> bool {
    !shared::costs_match(usage, &costs.models)
}

fn publish_calendar(
    sender: &watch::Sender<shared::LocalHistorySnapshot>,
    calendar: &crate::local_history::LocalHistory,
    usage_revisions: &[(String, u64)],
    generation: u64,
    loading: bool,
) {
    let revision = sender.borrow().revision + 1;
    sender.send_replace(shared::LocalHistorySnapshot {
        generation,
        revision,
        calendar_id: calendar.window.id.clone(),
        loading,
        details: calendar.view(),
        exact_costs: calendar.exact_costs(),
        usage_revisions: usage_revisions.to_vec(),
    });
}

/// Une erreur disque est journalisée par l'écrivain, jamais traitée comme un échec réseau :
/// elle forcerait sinon un rechargement complet du calendrier.
fn persist_calendar(
    writer: &Option<crate::persistence::Writer<crate::local_history::LocalHistory>>,
    calendar: &crate::local_history::LocalHistory,
) {
    if let Some(writer) = writer {
        writer.save(calendar.clone());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn key_event_sets_and_clears_whole_proxy_scope() {
        let mut stats = shared::Stats::default();
        for whole_proxy in [true, false] {
            super::apply(
                &mut stats,
                super::Event::Key(crate::litellm::KeyData {
                    period_spend: 2.0,
                    whole_proxy,
                    max_budget: None,
                    budget_reset_at: None,
                    period_start: None,
                }),
            );
            assert_eq!(stats.whole_proxy, whole_proxy);
        }
    }
    #[tokio::test]
    async fn settings_change_drops_queued_today_refresh_token() {
        let old = MockServer::start();
        let new = MockServer::start();
        let today = Local::now().date_naive();
        old.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200)
                .delay(Duration::from_millis(300))
                .json_body(daily_page(today, 1.0));
        });
        mocks(&old, 42.0, 0);
        mocks(&new, 7.0, 0);
        new.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200).json_body(daily_page(today, 9.0));
        });
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&old.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        service.refresh_and_wait().await.unwrap();
        service.save(preferences(&new, false)).await.unwrap();
        let stats = finished(&service, 3).await;
        assert!(stats.error.is_none());
        assert_eq!(stats.today[0].spend, 9.0);
        task.abort();
    }

    #[tokio::test]
    async fn today_selected_and_budget_refresh_share_one_forced_download() {
        let server = MockServer::start();
        let today = Local::now().date_naive();
        let mut initial = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200).json_body(daily_page(today, 1.0));
        });
        mocks(&server, 42.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        finished(&service, 1).await;
        initial.delete();
        let corrected = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200)
                .delay(Duration::from_millis(100))
                .json_body(daily_page(today, 9.0));
        });
        let id = service
            .begin_day_refresh(today.to_string())
            .await
            .unwrap()
            .data;
        service.refresh_with_day(Some(id)).await.unwrap();
        let detail = service
            .day_usage_in_refresh(today.to_string(), true, Some(id))
            .await
            .unwrap();
        let stats = finished(&service, 2).await;
        assert_eq!(detail.data.models, stats.today);
        assert_eq!(stats.today[0].spend, 9.0);
        corrected.assert_hits(1);
        // Un consommateur plus tardif du même clic reprend aussi le résultat terminé.
        assert_eq!(
            service
                .day_usage_in_refresh(today.to_string(), true, Some(id))
                .await
                .unwrap()
                .data,
            detail.data
        );
        corrected.assert_hits(1);
        task.abort();
    }

    #[tokio::test]
    async fn shared_daily_failure_can_retry_without_retaining_failed_future() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(200)
                .json_body(serde_json::json!({"info":{"user_id":"test-user"}}));
        });
        let day = Local::now().date_naive() - chrono::Duration::days(3);
        let mut failed = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(day));
            then.status(503).delay(Duration::from_millis(100));
        });
        let client = Client::new(&server.base_url(), "sk-test".into()).unwrap();
        let date = day.to_string();
        let (a, b) = tokio::join!(
            client.fetch_day_shared(&date, false, None),
            client.fetch_day_shared(&date, false, None)
        );
        assert!(a.is_err() && b.is_err());
        failed.assert_hits(1);
        failed.delete();
        let valid = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(day));
            then.status(200).json_body(daily_page(day, 3.0));
        });
        assert_eq!(
            client
                .fetch_day_shared(&date, false, None)
                .await
                .unwrap()
                .data
                .models[0]
                .spend,
            3.0
        );
        valid.assert_hits(1);
    }

    fn daily_page(day: chrono::NaiveDate, spend: f64) -> serde_json::Value {
        serde_json::json!({"data":[{"request_id":format!("{day}-{spend}"),"model":"m","spend":spend,"prompt_tokens":8,"completion_tokens":2,"total_tokens":10,"startTime":day.and_hms_opt(12,0,0).unwrap().and_local_timezone(Local).earliest().unwrap().to_rfc3339()}],"page":1,"page_size":1000,"total":1,"total_pages":1})
    }
    fn daily_start(day: chrono::NaiveDate) -> String {
        crate::litellm::calendar::day_bounds(day, &Local)
            .unwrap()
            .start
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }
    async fn wait_daily_request(mock: &httpmock::Mock<'_>) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while mock.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn startup_today_cannot_publish_after_selected_force_succeeds_or_fails() {
        for success in [true, false] {
            let server = MockServer::start();
            let today = Local::now().date_naive();
            let mut old = server.mock(|when, then| {
                when.path("/spend/logs/v2")
                    .query_param("start_date", daily_start(today));
                then.status(200)
                    .delay(Duration::from_millis(300))
                    .json_body(daily_page(today, 2.0));
            });
            mocks(&server, 42.0, 0);
            let (service, task) = start(
                Arc::new(SettingsStore::for_test(&server.base_url())),
                Arc::new(AtomicUsize::new(0)),
                None,
            );
            wait_daily_request(&old).await;
            old.delete();
            server.mock(|when, then| {
                when.path("/spend/logs/v2")
                    .query_param("start_date", daily_start(today));
                then.status(if success { 200 } else { 503 })
                    .json_body(daily_page(today, 9.0));
            });
            assert_eq!(
                service.day_usage(today.to_string(), true).await.is_ok(),
                success
            );
            let stats = finished(&service, 1).await;
            let cached = service.cached_day(today.to_string()).await.unwrap().data;
            if success {
                assert_eq!(stats.today[0].spend, 9.0);
                assert!(stats.error.is_none());
                assert_eq!(cached.unwrap().usage.unwrap().models[0].spend, 9.0);
            } else {
                assert!(stats.today.is_empty());
                assert!(cached.is_none());
            }
            task.abort();
        }
    }

    #[tokio::test]
    async fn yesterday_click_joins_preload_and_today_cycle_updates_exact_calendar() {
        let server = MockServer::start();
        let today = Local::now().date_naive();
        let yesterday = today.pred_opt().unwrap();
        let mut current = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200).json_body(daily_page(today, 2.0));
        });
        let preload = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(yesterday));
            then.status(200)
                .delay(Duration::from_millis(300))
                .json_body(daily_page(yesterday, 3.0));
        });
        mocks(&server, 42.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        finished(&service, 1).await;
        wait_daily_request(&preload).await;
        assert_eq!(
            service
                .day_usage(yesterday.to_string(), false)
                .await
                .unwrap()
                .data
                .models[0]
                .spend,
            3.0
        );
        preload.assert_hits(1);
        service.day_usage(today.to_string(), true).await.unwrap();
        current.delete();
        server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200).json_body(daily_page(today, 9.0));
        });
        service.refresh_and_wait().await.unwrap();
        finished(&service, 2).await;
        let calendar = service.local_history(HistoryMode::Cached).await.unwrap();
        assert_eq!(
            calendar
                .exact_costs
                .iter()
                .find(|d| d.date == today.to_string())
                .unwrap()
                .spend,
            Some(9.0)
        );
        task.abort();
    }

    #[tokio::test]
    async fn global_force_supersedes_older_selected_today_result() {
        let server = MockServer::start();
        let today = Local::now().date_naive();
        let mut current = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200).json_body(daily_page(today, 1.0));
        });
        mocks(&server, 42.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        finished(&service, 1).await;
        current.delete();
        let mut old = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200)
                .delay(Duration::from_millis(300))
                .json_body(daily_page(today, 2.0));
        });
        let cloned = service.clone();
        let selected = tokio::spawn(async move { cloned.day_usage(today.to_string(), true).await });
        wait_daily_request(&old).await;
        old.delete();
        server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", daily_start(today));
            then.status(200).json_body(daily_page(today, 9.0));
        });
        service.refresh_and_wait().await.unwrap();
        let stats = finished(&service, 2).await;
        assert_eq!(stats.today[0].spend, 9.0);
        assert!(selected.await.unwrap().is_err());
        task.abort();
    }

    #[tokio::test]
    async fn startup_today_and_yesterday_share_daily_cache_with_first_detail_click() {
        let server = MockServer::start();
        let today = Local::now().date_naive();
        let yesterday = today.pred_opt().unwrap();
        let page = |day: chrono::NaiveDate, spend| serde_json::json!({"data":[{"request_id":day.to_string(),"model":"m","spend":spend,"prompt_tokens":80,"completion_tokens":20,"total_tokens":100,"startTime":day.and_hms_opt(12,0,0).unwrap().and_local_timezone(Local).earliest().unwrap().to_rfc3339()}],"page":1,"page_size":1000,"total":1,"total_pages":1});
        let mut downloads = Vec::new();
        for (day, spend) in [(today, 2.0), (yesterday, 3.0)] {
            let bounds = crate::litellm::calendar::day_bounds(day, &Local).unwrap();
            downloads.push(server.mock(|when, then| {
                let when = when.path("/spend/logs/v2").query_param(
                    "start_date",
                    bounds.start.format("%Y-%m-%d %H:%M:%S").to_string(),
                );
                // La journée en cours s'arrête à l'instant de la requête.
                if day != today {
                    when.query_param(
                        "end_date",
                        bounds.end.format("%Y-%m-%d %H:%M:%S").to_string(),
                    );
                }
                then.status(200).json_body(page(day, spend));
            }));
        }
        mocks(&server, 42.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let stats = finished(&service, 1).await;
        assert_eq!(stats.today[0].spend, 2.0);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if service
                    .cached_day(yesterday.to_string())
                    .await
                    .unwrap()
                    .data
                    .is_some_and(|c| c.usage.is_some())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("yesterday must be prefetched after startup");
        assert_eq!(
            service
                .day_usage(today.to_string(), false)
                .await
                .unwrap()
                .data
                .models[0]
                .spend,
            2.0
        );
        assert_eq!(
            service
                .day_costs(today.to_string(), false)
                .await
                .unwrap()
                .data
                .models[0]
                .spend,
            2.0
        );
        assert_eq!(
            service
                .day_usage(yesterday.to_string(), false)
                .await
                .unwrap()
                .data
                .models[0]
                .spend,
            3.0
        );
        for download in downloads {
            download.assert_hits(1);
        }
        task.abort();
    }

    #[tokio::test]
    async fn startup_prefetch_does_not_delay_budget_and_manual_refresh_updates_today() {
        let server = MockServer::start();
        let today = Local::now().date_naive();
        let yesterday = today.pred_opt().unwrap();
        let start_date = |day| {
            crate::litellm::calendar::day_bounds(day, &Local)
                .unwrap()
                .start
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        };
        let mut current = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", start_date(today));
            then.status(200).json_body(
                serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}),
            );
        });
        let old=server.mock(|when,then|{when.path("/spend/logs/v2").query_param("start_date",start_date(yesterday));then.status(200).delay(Duration::from_millis(500)).json_body(serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}));});
        mocks(&server, 42.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let stats = finished(&service, 1).await;
        assert_eq!(stats.period_spend, 42.0);
        assert!(service
            .cached_day(yesterday.to_string())
            .await
            .unwrap()
            .data
            .is_none());
        current.delete();
        server.mock(|when,then|{when.path("/spend/logs/v2").query_param("start_date",start_date(today));then.status(200).json_body(serde_json::json!({"data":[{"request_id":"updated","model":"m","spend":9,"prompt_tokens":8,"completion_tokens":2,"total_tokens":10,"startTime":Local::now().to_rfc3339()}],"page":1,"page_size":1000,"total":1,"total_pages":1}));});
        service.refresh_and_wait().await.unwrap();
        let refreshed = finished(&service, 2).await;
        assert_eq!(refreshed.today[0].spend, 9.0);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if service
                    .cached_day(yesterday.to_string())
                    .await
                    .unwrap()
                    .data
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        old.assert_hits(1);
        task.abort();
    }

    #[tokio::test]
    async fn startup_prefetch_reuses_persisted_yesterday_and_isolates_new_source() {
        let first = MockServer::start();
        let second = MockServer::start();
        let today = Local::now().date_naive();
        let yesterday = today.pred_opt().unwrap();
        let bounds = crate::litellm::calendar::day_bounds(yesterday, &Local).unwrap();
        let store = Arc::new(SettingsStore::for_test(&first.base_url()));
        store.save(preferences(&first, false)).unwrap();
        let directory = std::env::temp_dir().join(format!("startup-days-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut persisted = crate::daily_store::DailyStore::load(None, store.get().source_id);
        persisted.put(
            bounds,
            None,
            Some(shared::DailyUsage {
                date: yesterday.to_string(),
                models: vec![ModelUsage {
                    model: "m".into(),
                    spend: 3.0,
                    total_tokens: 10,
                    ..Default::default()
                }],
            }),
            chrono::Utc::now(),
        );
        crate::persistence::write(&directory.join("daily-usage.json"), &persisted).unwrap();
        let first_day = first.mock(|when, then| {
            when.path("/spend/logs/v2").query_param(
                "start_date",
                bounds.start.format("%Y-%m-%d %H:%M:%S").to_string(),
            );
            then.status(503);
        });
        for server in [&first, &second] {
            mocks(server, 42.0, 0);
            let start = crate::litellm::calendar::day_bounds(today, &Local)
                .unwrap()
                .start
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            server.mock(|when,then|{when.path("/spend/logs/v2").query_param("start_date",start);then.status(200).json_body(serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}));});
        }
        let new_day = second.mock(|when, then| {
            when.path("/spend/logs/v2").query_param(
                "start_date",
                bounds.start.format("%Y-%m-%d %H:%M:%S").to_string(),
            );
            then.status(200).json_body(
                serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}),
            );
        });
        let (service, task) = start_with_storage(
            store,
            StoragePaths {
                stats: None,
                history: Some(directory.join("history.json")),
            },
            Arc::new(|_, _| Ok(false)),
        );
        finished(&service, 1).await;
        assert_eq!(
            service
                .day_usage(yesterday.to_string(), false)
                .await
                .unwrap()
                .data
                .models[0]
                .spend,
            3.0
        );
        first_day.assert_hits(0);
        service.save(preferences(&second, false)).await.unwrap();
        finished(&service, 2).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if service
                    .cached_day(yesterday.to_string())
                    .await
                    .unwrap()
                    .data
                    .is_some_and(|c| c.usage.is_some())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(service
            .day_usage(yesterday.to_string(), false)
            .await
            .unwrap()
            .data
            .models
            .is_empty());
        new_day.assert_hits(1);
        first_day.assert_hits(0);
        task.abort();
        // Un cycle encore en cours peut écrire après coup : nettoyage au mieux.
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn daily_cached_revisit_avoids_http_after_rounding_only_preview() {
        let server = MockServer::start();
        mocks(&server, 1.0, 0);
        let logs = server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(503);
        });
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let day = Local::now().date_naive().pred_opt().unwrap();
        let date = day.to_string();
        let (_, state, attempt, _) = service
            .request(|reply| Command::DailyContext(date.clone(), None, reply))
            .await
            .unwrap();
        let bounds = crate::litellm::calendar::day_bounds(day, &Local).unwrap();
        let now = chrono::Utc::now() - chrono::Duration::seconds(1);
        let usage = shared::DailyUsage {
            date: date.clone(),
            models: vec![ModelUsage {
                model: "m".into(),
                spend: 23.301333660000004,
                total_tokens: 100,
                ..Default::default()
            }],
        };
        service
            .request(|reply| {
                Command::StoreDaily(
                    state.generation,
                    attempt,
                    bounds,
                    None,
                    Some(usage.clone()),
                    now,
                    reply,
                )
            })
            .await
            .unwrap();
        let costs = shared::DailyCosts {
            date: date.clone(),
            models: vec![ModelCost {
                model: "m".into(),
                spend: 23.30133365999999,
            }],
        };
        service
            .request(|reply| {
                Command::StoreDaily(
                    state.generation,
                    attempt,
                    bounds,
                    Some(costs),
                    None,
                    now + chrono::Duration::milliseconds(3),
                    reply,
                )
            })
            .await
            .unwrap();
        assert_eq!(
            service.day_usage(date.clone(), false).await.unwrap().data,
            usage
        );
        assert_eq!(service.day_usage(date, false).await.unwrap().data, usage);
        logs.assert_hits(0);
        task.abort();
    }

    #[tokio::test]
    async fn daily_incompatible_usage_bypasses_retained_memory() {
        let server = MockServer::start();
        let day = Local::now().date_naive().pred_opt().unwrap();
        let date = day.to_string();
        let timestamp = day
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap()
            .to_rfc3339();
        server.mock(|when, then| {
            when.path("/spend/logs").query_param("summarize", "true");
            then.status(200)
                .json_body(serde_json::json!([{"startTime":date,"spend":9,"models":{"m":9}}]));
        });
        let mut logs=server.mock(|when,then|{when.path("/spend/logs/v2");then.status(200).json_body(serde_json::json!({"data":[{"request_id":"a","model":"m","spend":2,"prompt_tokens":8,"completion_tokens":2,"total_tokens":10,"startTime":timestamp}],"page":1,"page_size":1000,"total":1,"total_pages":1}));});
        mocks(&server, 1.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        assert_eq!(
            service
                .day_usage(date.clone(), false)
                .await
                .unwrap()
                .data
                .models[0]
                .spend,
            2.0
        );
        logs.delete();
        // Les corrections de minuit restent disponibles ; seul le téléchargement de la journée échoue.
        let bounds = crate::litellm::calendar::day_bounds(day, &Local).unwrap();
        let failed = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param(
                    "start_date",
                    bounds.start.format("%Y-%m-%d %H:%M:%S").to_string(),
                )
                .query_param(
                    "end_date",
                    bounds.end.format("%Y-%m-%d %H:%M:%S").to_string(),
                );
            then.status(503);
        });
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200).json_body(
                serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}),
            );
        });
        let id = service.begin_day_refresh(date.clone()).await.unwrap().data;
        let (costs, usage) = tokio::join!(
            service.day_costs_in_refresh(date.clone(), true, Some(id)),
            service.day_usage_in_refresh(date.clone(), true, Some(id))
        );
        assert_eq!(costs.unwrap().data.models[0].spend, 9.0);
        assert!(usage.is_err());
        assert!(
            service
                .cached_day(date.clone())
                .await
                .unwrap()
                .data
                .unwrap()
                .usage_stale
        );
        assert!(service.day_usage(date.clone(), false).await.is_err());
        failed.assert_hits(2);
        assert_eq!(
            service.day_costs(date, false).await.unwrap().data.models[0].spend,
            9.0
        );
        task.abort();
    }

    use super::*;
    use httpmock::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn daily_response_rejects_changed_timezone_bounds() {
        let day = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let utc = crate::litellm::calendar::day_bounds(day, &chrono::Utc);
        let east = crate::litellm::calendar::day_bounds(
            day,
            &chrono::FixedOffset::east_opt(7200).unwrap(),
        );
        assert!(validate_daily_bounds(utc, utc).is_ok());
        assert!(validate_daily_bounds(utc, east).is_err());
        assert!(validate_daily_bounds(utc, None).is_err());
    }

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
    pub(crate) fn start(
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

    pub(crate) fn start_with_notify(
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
        let (local_snapshots, local_updates) =
            watch::channel(shared::LocalHistorySnapshot::default());
        let service = Service {
            commands: commands.clone(),
            snapshots: updates,
            local_snapshots: local_updates,
        };
        let task = tokio::spawn(coordinate(
            store,
            paths,
            notify,
            |_| {},
            commands,
            receiver,
            (snapshots, local_snapshots),
        ));
        (service, task)
    }
    /// Enregistre un coût de journée par le chemin de production : jeton de rafraîchissement,
    /// puis `StoreDaily`, comme le fait `day_costs_in_refresh`.
    async fn store_cost(
        service: &Service,
        generation: u64,
        bounds: crate::litellm::calendar::DayBounds,
        spend: f64,
        when: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), String> {
        let date = bounds.date.to_string();
        let attempt = service.begin_day_refresh(date.clone()).await?.data;
        let costs = shared::DailyCosts {
            date,
            models: vec![ModelCost {
                model: "m".into(),
                spend,
            }],
        };
        service
            .request(|reply| {
                Command::StoreDaily(generation, attempt, bounds, Some(costs), None, when, reply)
            })
            .await
    }
    pub(crate) async fn finished(service: &Service, minimum_cycle: u64) -> Stats {
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
    async fn historical_cache_survives_automatic_budget_refresh() {
        let server = MockServer::start();
        let range_start = crate::litellm::calendar::day_bounds(
            chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
            &Local,
        )
        .unwrap()
        .start
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
        let historical=server.mock(|when,then| {
            when.path("/spend/logs/v2").query_param("start_date",range_start);
            then.status(200).json_body(serde_json::json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false}));
        });
        mocks(&server, 42.0, 0);
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        let mut settings = preferences(&server, false);
        settings.refresh_secs = 5;
        store.save(settings).unwrap();
        let (service, task) = start(store, Arc::new(AtomicUsize::new(0)), None);
        let before = finished(&service, 1).await;
        service.day_usage("2024-02-29".into(), false).await.unwrap();
        let mut snapshots = service.snapshots.clone();
        tokio::time::timeout(
            Duration::from_secs(7),
            snapshots.wait_for(|s| s.cycle_id > before.cycle_id && !s.refreshing),
        )
        .await
        .unwrap()
        .unwrap();
        service.day_usage("2024-02-29".into(), false).await.unwrap();
        historical.assert_hits(1);
        task.abort();
    }

    #[tokio::test]
    async fn historical_cache_survives_current_budget_refresh() {
        let server = MockServer::start();
        let range_start = crate::litellm::calendar::day_bounds(
            chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
            &Local,
        )
        .unwrap()
        .start
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
        let historical=server.mock(|when,then| {
            when.path("/spend/logs/v2").query_param("start_date",range_start);
            then.status(200).json_body(serde_json::json!({
                "data":[],"total":0,"page":1,"page_size":1000,"total_pages":0,"total_is_capped":false
            }));
        });
        mocks(&server, 42.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let before = finished(&service, 1).await;
        service.day_usage("2024-02-29".into(), false).await.unwrap();
        service.refresh_and_wait().await.unwrap();
        finished(&service, before.cycle_id + 1).await;
        service.day_usage("2024-02-29".into(), false).await.unwrap();
        historical.assert_hits(1);
        task.abort();
    }

    #[tokio::test]
    async fn historical_usage_does_not_modify_live_stats() {
        let server = MockServer::start();
        mocks(&server, 42.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let before = finished(&service, 1).await;
        let historical = service.day_usage("2024-02-29".into(), false).await.unwrap();
        assert_eq!(historical.generation, before.generation);
        assert_eq!(historical.data.date, "2024-02-29");
        assert_eq!(service.stats(), before);
        task.abort();
    }

    #[tokio::test]
    async fn historical_usage_rejects_old_connection_response() {
        let old = MockServer::start();
        let new = MockServer::start();
        let historical = old.mock(|when, then| {
            when.path("/spend/logs")
                .query_param("summarize", "false")
                .query_param_exists("start_date")
                .matches(|request| {
                    request.query_params.as_ref().is_some_and(|params| {
                        params
                            .iter()
                            .any(|(key, value)| key == "start_date" && value.starts_with("2024-"))
                    })
                });
            then.status(200)
                .delay(Duration::from_millis(500))
                .json_body(serde_json::json!([]));
        });
        mocks(&old, 99.0, 0);
        mocks(&new, 7.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&old.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        finished(&service, 1).await;
        let pending_service = service.clone();
        let pending =
            tokio::spawn(
                async move { pending_service.day_usage("2024-02-29".into(), false).await },
            );
        tokio::time::timeout(Duration::from_secs(2), async {
            while historical.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        service.save(preferences(&new, false)).await.unwrap();
        assert!(matches!(pending.await.unwrap(), Err(error) if error.contains("configuration")));
        task.abort();
    }

    #[tokio::test]
    async fn local_calendar_publishes_progress_and_keeps_utc_history_independent() {
        let server = MockServer::start();
        let today = Local::now().date_naive();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(
                serde_json::json!([{"startTime":today.to_string(),"spend":7,"models":{"m":7}}]),
            );
        });
        let details = server.mock(|when, then| {
            when.path("/spend/logs/v2").query_param(
                "start_date",
                (today - chrono::Duration::days(7))
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_local_timezone(Local)
                    .earliest()
                    .unwrap()
                    .with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            );
            then.status(200).json_body(
                serde_json::json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}),
            );
        });
        mocks(&server, 1.0, 0);
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let (service, task) = start(store, Arc::new(AtomicUsize::new(0)), None);
        let mut updates = service.subscribe_local_history();
        let cached = service.local_history(HistoryMode::Cached).await.unwrap();
        assert!(!cached.loading);
        let initial = service.local_history(HistoryMode::Refresh).await.unwrap();
        assert!(initial.loading);
        let complete = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                updates.changed().await.unwrap();
                let next = updates.borrow().clone();
                if next.revision > initial.revision && !next.loading {
                    break next;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(complete.details.days[29].spend, Some(7.0));
        details.assert_hits(0);
        assert!(complete.details.error.is_none());
        assert!(service
            .history(HistoryMode::Cached)
            .await
            .unwrap()
            .data
            .days
            .iter()
            .all(|d| d.spend.is_none()));
        task.abort();
    }

    #[tokio::test]
    async fn local_calendar_force_supersedes_and_selected_cost_beats_older_batch() {
        let server = MockServer::start();
        let today = Local::now().date_naive();
        let mut old = server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200)
                .delay(Duration::from_millis(250))
                .json_body(
                    serde_json::json!([{"startTime":today.to_string(),"spend":1,"models":{"m":1}}]),
                );
        });
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200).json_body(
                serde_json::json!({"data":[],"total":0,"page":1,"page_size":1000,"total_pages":0}),
            );
        });
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(200)
                .json_body(serde_json::json!({"info":{"spend":1.0}}));
        });
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let (service, task) = start(store, Arc::new(AtomicUsize::new(0)), None);
        let mut updates = service.subscribe_local_history();
        let first = service.local_history(HistoryMode::Refresh).await.unwrap();
        let shared = service.local_history(HistoryMode::Refresh).await.unwrap();
        assert_eq!(shared.revision, first.revision);
        tokio::time::timeout(Duration::from_secs(2), async {
            while old.hits_async().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        old.delete();
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(200).json_body(
                serde_json::json!([{"startTime":today.to_string(),"spend":2,"models":{"m":2}}]),
            );
        });
        let forced = service.local_history(HistoryMode::Force).await.unwrap();
        assert!(forced.revision > first.revision);
        let bounds = crate::litellm::calendar::day_bounds(today, &Local).unwrap();
        store_cost(
            &service,
            forced.generation,
            bounds,
            9.0,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        let complete = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                updates.changed().await.unwrap();
                let value = updates.borrow().clone();
                // Le cycle de démarrage publie aussi le coût du jour : attendre la valeur injectée.
                if value.revision > forced.revision
                    && !value.loading
                    && value.exact_costs.iter().any(|d| d.spend == Some(9.0))
                {
                    break value;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(complete.details.days[29].spend, Some(2.0));
        assert_eq!(complete.exact_costs[0].spend, Some(9.0));
        // Une génération plus récente que la connexion courante est refusée.
        assert!(store_cost(
            &service,
            forced.generation + 1,
            bounds,
            99.0,
            chrono::Utc::now() + chrono::Duration::hours(2),
        )
        .await
        .is_err());
        let current = service.local_history(HistoryMode::Cached).await.unwrap();
        assert_eq!(current.exact_costs[0].spend, Some(9.0));
        task.abort();
    }

    #[tokio::test]
    async fn daily_and_calendar_writes_run_behind_the_coordinator() {
        let server = MockServer::start();
        mocks(&server, 1.0, 0);
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200).json_body(
                serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}),
            );
        });
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let directory =
            std::env::temp_dir().join(format!("background-writes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let (service, task) = start_with_storage(
            store,
            StoragePaths {
                stats: Some(directory.join("stats.json")),
                history: Some(directory.join("history.json")),
            },
            Arc::new(|_, _| Ok(false)),
        );
        let first = finished(&service, 1).await;
        // Disque bloqué : le coordinateur doit répondre sans attendre les écritures.
        crate::persistence::tests::hold_writes_in(&directory);
        let day = Local::now().date_naive() - chrono::Duration::days(3);
        let days = [day, day - chrono::Duration::days(1)];
        for day in days {
            tokio::time::timeout(
                Duration::from_secs(2),
                service.day_usage(day.to_string(), false),
            )
            .await
            .expect("StoreDaily must not wait for the disk")
            .unwrap();
        }
        service.refresh_and_wait().await.unwrap();
        let second = finished(&service, first.cycle_id + 1).await;
        // Le cycle de démarrage a pu écrire avant le blocage, jamais les journées demandées ici.
        let held = std::fs::read_to_string(directory.join("daily-usage.json")).unwrap_or_default();
        assert!(days.iter().all(|d| !held.contains(&d.to_string())));
        crate::persistence::tests::release_writes_in(&directory);
        service.flush().await;
        let daily = std::fs::read_to_string(directory.join("daily-usage.json")).unwrap();
        let calendar = std::fs::read_to_string(directory.join("history-local.json")).unwrap();
        for day in days {
            assert!(daily.contains(&day.to_string()) && calendar.contains(&day.to_string()));
        }
        let saved: Stats =
            serde_json::from_slice(&std::fs::read(directory.join("stats.json")).unwrap()).unwrap();
        assert_eq!(saved.cycle_id, second.cycle_id);
        task.abort();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn newer_today_costs_hide_contradicted_live_tokens() {
        let server = MockServer::start();
        mocks(&server, 1.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let first = finished(&service, 1).await;
        let day = Local::now().date_naive();
        let bounds = crate::litellm::calendar::day_bounds(day, &Local).unwrap();
        let usage = shared::DailyUsage {
            date: day.to_string(),
            models: vec![ModelUsage {
                model: "m".into(),
                spend: 2.0,
                total_tokens: 100,
                ..Default::default()
            }],
        };
        let store_usage = |when| {
            let service = service.clone();
            let usage = usage.clone();
            async move {
                let attempt = service.begin_day_refresh(day.to_string()).await?.data;
                service
                    .request(|reply| {
                        Command::StoreDaily(
                            first.generation,
                            attempt,
                            bounds,
                            None,
                            Some(usage),
                            when,
                            reply,
                        )
                    })
                    .await
            }
        };
        // Instants passés mais postérieurs au cycle de démarrage, sans dater le cache dans le futur.
        let t0 = chrono::Utc::now();
        tokio::time::sleep(Duration::from_millis(30)).await;
        store_usage(t0).await.unwrap();
        assert_eq!(service.snapshots.borrow().today[0].total_tokens, 100);
        // Coûts corrigés plus récents : les tokens du jour ne peuvent plus s'afficher.
        store_cost(
            &service,
            first.generation,
            bounds,
            9.0,
            t0 + chrono::Duration::milliseconds(20),
        )
        .await
        .unwrap();
        assert!(service.snapshots.borrow().today.is_empty());
        assert_eq!(service.snapshots.borrow().today_date, None);
        // Des tokens validés avant ces coûts (cache mémoire) ne reviennent pas.
        let _ = store_usage(t0 + chrono::Duration::milliseconds(10)).await;
        assert!(service.snapshots.borrow().today.is_empty());
        task.abort();
    }

    #[tokio::test]
    async fn restored_today_tokens_yield_to_newer_corrected_costs() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(200)
                .json_body(serde_json::json!({"info":{"spend":1.0}}));
        });
        // Les tokens du jour ne se rechargent pas : seuls les tokens restaurés existent.
        server.mock(|when, then| {
            when.path("/spend/logs");
            then.status(503);
        });
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(503);
        });
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        let directory =
            std::env::temp_dir().join(format!("restored-today-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let day = Local::now().date_naive();
        let restored = Stats {
            source_id: store.get().source_id,
            today_date: Some(day.to_string()),
            today: vec![ModelUsage {
                model: "m".into(),
                spend: 2.0,
                total_tokens: 100,
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            directory.join("stats.json"),
            serde_json::to_vec(&restored).unwrap(),
        )
        .unwrap();
        let (service, task) = start_with_storage(
            store,
            StoragePaths {
                stats: Some(directory.join("stats.json")),
                history: None,
            },
            Arc::new(|_, _| Ok(false)),
        );
        let first = finished(&service, 1).await;
        assert_eq!(first.today[0].total_tokens, 100);
        let bounds = crate::litellm::calendar::day_bounds(day, &Local).unwrap();
        store_cost(&service, first.generation, bounds, 9.0, chrono::Utc::now())
            .await
            .unwrap();
        assert!(service.snapshots.borrow().today.is_empty());
        task.abort();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn restart_reconciles_restored_today_tokens_with_persisted_costs() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/key/info");
            then.status(200)
                .json_body(serde_json::json!({"info":{"spend":1.0}}));
        });
        for path in ["/spend/logs", "/spend/logs/v2"] {
            server.mock(|when, then| {
                when.path(path);
                then.status(503);
            });
        }
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        let directory =
            std::env::temp_dir().join(format!("restart-today-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let paths = || StoragePaths {
            stats: Some(directory.join("stats.json")),
            history: Some(directory.join("history.json")),
        };
        let day = Local::now().date_naive();
        let bounds = crate::litellm::calendar::day_bounds(day, &Local).unwrap();
        // Premier lancement : seuls les coûts corrigés atteignent le disque.
        let (service, task) =
            start_with_storage(store.clone(), paths(), Arc::new(|_, _| Ok(false)));
        let first = finished(&service, 1).await;
        store_cost(&service, first.generation, bounds, 9.0, chrono::Utc::now())
            .await
            .unwrap();
        service.flush().await;
        task.abort();
        let stale = Stats {
            source_id: store.get().source_id,
            today_date: Some(day.to_string()),
            today: vec![ModelUsage {
                model: "m".into(),
                spend: 2.0,
                total_tokens: 100,
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            directory.join("stats.json"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        let (service, task) = start_with_storage(store, paths(), Arc::new(|_, _| Ok(false)));
        let restarted = finished(&service, 1).await;
        assert!(restarted.today.is_empty());
        assert_eq!(restarted.today_date, None);
        task.abort();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn daily_responses_carry_their_validation_instant() {
        let server = MockServer::start();
        mocks(&server, 1.0, 0);
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200).json_body(serde_json::json!({
                "data": [], "total": 0, "page": 1, "page_size": 1000,
                "total_pages": 0, "total_is_capped": false
            }));
        });
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        finished(&service, 1).await;
        let day = (Local::now().date_naive() - chrono::Duration::days(3)).to_string();
        let before = chrono::Utc::now().timestamp_micros();
        let costs = service.day_costs(day.clone(), true).await.unwrap();
        let usage = service.day_usage(day.clone(), true).await.unwrap();
        let after = chrono::Utc::now().timestamp_micros();
        for at in [costs.validated_at, usage.validated_at] {
            let at = at.expect("instant de validation");
            assert!((before - 1_000_000..=after).contains(&at));
        }
        // Le cache renvoie l'instant de sa dernière validation, pas celui de la lecture.
        let cached = service.day_costs(day, false).await.unwrap();
        assert_eq!(cached.validated_at, usage.validated_at);
        task.abort();
    }

    #[tokio::test]
    async fn token_only_changes_publish_a_new_usage_revision() {
        let server = MockServer::start();
        mocks(&server, 1.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let first = finished(&service, 1).await;
        let day = Local::now().date_naive() - chrono::Duration::days(3);
        let bounds = crate::litellm::calendar::day_bounds(day, &Local).unwrap();
        let usage = |tokens| shared::DailyUsage {
            date: day.to_string(),
            models: vec![ModelUsage {
                model: "free".into(),
                spend: 0.0,
                total_tokens: tokens,
                ..Default::default()
            }],
        };
        let store = |tokens, when| {
            let service = service.clone();
            async move {
                let attempt = service.begin_day_refresh(day.to_string()).await?.data;
                service
                    .request(|reply| {
                        Command::StoreDaily(
                            first.generation,
                            attempt,
                            bounds,
                            None,
                            Some(usage(tokens)),
                            when,
                            reply,
                        )
                    })
                    .await
            }
        };
        let now = chrono::Utc::now();
        store(100, now).await.unwrap();
        let before = service.local_history(HistoryMode::Cached).await.unwrap();
        let revision = |s: &shared::LocalHistorySnapshot| {
            s.usage_revisions
                .iter()
                .find(|(d, _)| d == &day.to_string())
                .map(|(_, r)| *r)
        };
        assert!(revision(&before).is_some());
        // Coût identique (0 $), tokens doublés : le récapitulatif doit être invalidé.
        store(200, now + chrono::Duration::seconds(1))
            .await
            .unwrap();
        let after = service.local_history(HistoryMode::Cached).await.unwrap();
        assert!(after.revision > before.revision);
        assert!(revision(&after) > revision(&before));
        task.abort();
    }

    #[tokio::test]
    async fn local_calendar_disk_failure_keeps_memory_and_saved_connection_clears_it() {
        let old = MockServer::start();
        let new = MockServer::start();
        mocks(&old, 1.0, 0);
        mocks(&new, 2.0, 0);
        let store = Arc::new(SettingsStore::for_test(&old.base_url()));
        store.save(preferences(&old, false)).unwrap();
        let directory =
            std::env::temp_dir().join(format!("local-calendar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(directory.join("history-local.json")).unwrap();
        let (service, task) = start_with_storage(
            store,
            StoragePaths {
                stats: None,
                history: Some(directory.join("history.json")),
            },
            Arc::new(|_, _| Ok(false)),
        );
        let first = service.local_history(HistoryMode::Cached).await.unwrap();
        let bounds =
            crate::litellm::calendar::day_bounds(Local::now().date_naive(), &Local).unwrap();
        store_cost(&service, first.generation, bounds, 7.0, chrono::Utc::now())
            .await
            .unwrap();
        let current = service.local_history(HistoryMode::Cached).await.unwrap();
        assert_eq!(current.exact_costs[0].spend, Some(7.0));
        // L'échec d'écriture reste journalisé : il ne doit pas forcer un rechargement réseau.
        assert!(current.details.error.is_none());
        service.save(preferences(&new, false)).await.unwrap();
        let next = service.local_history(HistoryMode::Cached).await.unwrap();
        assert!(next.details.days.iter().all(|d| d.spend.is_none()));
        assert!(next.generation > first.generation);
        task.abort();
        // Un cycle encore en cours peut écrire après coup : nettoyage au mieux.
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn daily_refresh_attempts_share_cutoff_and_reject_obsolete_persistence() {
        let server = MockServer::start();
        mocks(&server, 1.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let date = Local::now().date_naive().pred_opt().unwrap().to_string();
        let first = service.begin_day_refresh(date.clone()).await.unwrap();
        let a = service
            .request(|reply| Command::DailyContext(date.clone(), Some(first.data), reply))
            .await
            .unwrap();
        let b = service
            .request(|reply| Command::DailyContext(date.clone(), Some(first.data), reply))
            .await
            .unwrap();
        assert_eq!(a.2, b.2);
        assert_eq!(a.3, b.3);
        let second = service.begin_day_refresh(date.clone()).await.unwrap();
        assert_ne!(first.data, second.data);
        assert!(service
            .request(|reply| Command::DailyContext(date.clone(), Some(first.data), reply))
            .await
            .is_err());
        let bounds = crate::litellm::calendar::day_bounds(
            chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").unwrap(),
            &Local,
        )
        .unwrap();
        assert!(service
            .request(|reply| Command::StoreDaily(
                first.generation,
                first.data,
                bounds,
                Some(shared::DailyCosts {
                    date: date.clone(),
                    models: vec![]
                }),
                None,
                chrono::Utc::now(),
                reply
            ))
            .await
            .is_err());
        assert!(service.cached_day(date).await.unwrap().data.is_none());
        task.abort();
    }

    #[tokio::test]
    async fn daily_memory_result_does_not_get_a_new_persisted_timestamp() {
        let server = MockServer::start();
        let day = Local::now().date_naive() - chrono::Duration::days(2);
        let range_start = crate::litellm::calendar::day_bounds(day, &Local)
            .unwrap()
            .start
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let logs = server.mock(|when, then| {
            when.path("/spend/logs/v2")
                .query_param("start_date", range_start);
            then.status(200).json_body(
                serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}),
            );
        });
        mocks(&server, 1.0, 0);
        let (service, task) = start(
            Arc::new(SettingsStore::for_test(&server.base_url())),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let date = day.to_string();
        let (client, _) = service.request(Command::Context).await.unwrap();
        let original = client
            .fetch_day_validated(&date, false, &Local, None)
            .await
            .unwrap();
        service.day_usage(date.clone(), false).await.unwrap();
        service.day_costs(date.clone(), false).await.unwrap();
        let cached = service.cached_day(date).await.unwrap().data.unwrap();
        assert_eq!(cached.fetched_at, original.fetched_at.to_rfc3339());
        logs.assert_hits(1);
        task.abort();
    }

    #[tokio::test]
    async fn daily_json_cache_survives_restart_and_failed_refresh_without_crossing_sources() {
        let server = MockServer::start();
        // Ce test redémarre le coordinateur dans le même processus : isoler ses écritures de celles du cycle automatique.
        let day = Local::now().date_naive() - chrono::Duration::days(2);
        let start = crate::litellm::calendar::day_bounds(day, &Local)
            .unwrap()
            .start
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let date = day.to_string();
        let timestamp = day
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap()
            .to_rfc3339();
        let mut logs=server.mock(|when,then| {
            when.path("/spend/logs/v2").query_param("start_date",start.clone());then.status(200).json_body(serde_json::json!({"data":[{"request_id":"valid","model":"m","spend":2,"prompt_tokens":8,"completion_tokens":2,"total_tokens":10,"startTime":timestamp}],"page":1,"page_size":1000,"total":1,"total_pages":1}));
        });
        mocks(&server, 1.0, 0);
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let directory =
            std::env::temp_dir().join(format!("daily-service-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let paths = || StoragePaths {
            stats: None,
            history: Some(directory.join("history.json")),
        };
        let (service, task) =
            start_with_storage(store.clone(), paths(), Arc::new(|_, _| Ok(false)));
        assert!(service
            .cached_day(date.clone())
            .await
            .unwrap()
            .data
            .is_none());
        let original = service.day_usage(date.clone(), false).await.unwrap().data;
        assert_eq!(original.models[0].total_tokens, 10);
        logs.assert_hits(1);
        service.flush().await;
        let json = std::fs::read_to_string(directory.join("daily-usage.json")).unwrap();
        assert!(!json.contains("request_id") && !json.contains("api_key") && !json.contains("sk-"));
        task.abort();
        let (service, task) =
            start_with_storage(store.clone(), paths(), Arc::new(|_, _| Ok(false)));
        let cached = service
            .cached_day(date.clone())
            .await
            .unwrap()
            .data
            .unwrap();
        assert!(!cached.stale);
        assert_eq!(cached.usage, Some(original.clone()));
        assert_eq!(
            service.day_usage(date.clone(), false).await.unwrap().data,
            original
        );
        logs.assert_hits(1);
        logs.delete();
        let truncated = server.mock(|when, then| {
            when.path("/spend/logs/v2").query_param("start_date", start);
            then.status(200).json_body(
                serde_json::json!({"data":[],"page":1,"page_size":1000,"total":1,"total_pages":1}),
            );
        });
        assert!(service.day_usage(date.clone(), true).await.is_err());
        truncated.assert_hits(1);
        assert_eq!(
            service
                .cached_day(date.clone())
                .await
                .unwrap()
                .data
                .unwrap()
                .usage,
            Some(original.clone())
        );
        let other = MockServer::start();
        mocks(&other, 2.0, 0);
        service.save(preferences(&other, false)).await.unwrap();
        assert!(service
            .cached_day(date.clone())
            .await
            .unwrap()
            .data
            .is_none());
        task.abort();
        // Une entrée expirée reste disponible comme aperçu, sans éviter la validation réseau.
        // Dossier distinct : `abort` n'annule pas une écriture disque déjà lancée par
        // le coordinateur précédent, qui pourrait écraser le fichier préparé ici.
        let root = directory;
        let directory = root.join("restart");
        std::fs::create_dir_all(&directory).unwrap();
        let paths = || StoragePaths {
            stats: None,
            history: Some(directory.join("history.json")),
        };
        let mut raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        raw["entries"][0]["fetched"] =
            serde_json::json!((chrono::Utc::now() - chrono::Duration::minutes(16)).to_rfc3339());
        raw["entries"][0]["usage_fetched"] = raw["entries"][0]["fetched"].clone();
        std::fs::write(
            directory.join("daily-usage.json"),
            serde_json::to_vec(&raw).unwrap(),
        )
        .unwrap();
        // L'identité du cache appartient au store d'origine, pas à une nouvelle configuration.
        store.save(preferences(&server, false)).unwrap();
        raw["source_id"] = serde_json::json!(store.get().source_id);
        std::fs::write(
            directory.join("daily-usage.json"),
            serde_json::to_vec(&raw).unwrap(),
        )
        .unwrap();
        let (service, task) = start_with_storage(store, paths(), Arc::new(|_, _| Ok(false)));
        let cached = service
            .cached_day(date.clone())
            .await
            .unwrap()
            .data
            .unwrap();
        assert!(cached.stale);
        assert!(service.day_usage(date.clone(), false).await.is_err());
        assert_eq!(
            service.cached_day(date).await.unwrap().data.unwrap().usage,
            Some(original)
        );
        task.abort();
        // Un cycle encore en cours peut écrire après coup : nettoyage au mieux.
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_persisted_day_revalidates_without_bypassing_analytics_backoff() {
        let server = MockServer::start();
        mocks(&server, 1.0, 0);
        let aggregate = server.mock(|when, then| {
            when.path("/user/daily/activity/aggregated");
            then.status(403);
        });
        server.mock(|when, then| {
            when.path("/spend/logs/v2");
            then.status(200).json_body(
                serde_json::json!({"data":[],"page":1,"page_size":1000,"total":0,"total_pages":0}),
            );
        });
        let day = Local::now().date_naive() - chrono::Duration::days(2);
        let store = Arc::new(SettingsStore::for_test(&server.base_url()));
        store.save(preferences(&server, false)).unwrap();
        let directory = std::env::temp_dir().join(format!("stale-daily-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut persisted = crate::daily_store::DailyStore::load(None, store.get().source_id);
        persisted.put(
            crate::litellm::calendar::day_bounds(day, &Local).unwrap(),
            None,
            Some(shared::DailyUsage {
                date: day.to_string(),
                models: vec![ModelUsage {
                    model: "m".into(),
                    spend: 3.0,
                    total_tokens: 10,
                    ..Default::default()
                }],
            }),
            chrono::Utc::now() - chrono::Duration::minutes(16),
        );
        crate::persistence::write(&directory.join("daily-usage.json"), &persisted).unwrap();
        let (service, task) = start_with_storage(
            store,
            StoragePaths {
                stats: None,
                history: Some(directory.join("history.json")),
            },
            Arc::new(|_, _| Ok(false)),
        );
        // Le cycle de démarrage essuie le refus et ouvre la période d'attente.
        finished(&service, 1).await;
        let denied = aggregate.hits();
        assert!(denied >= 1);
        let cached = service.cached_day(day.to_string()).await.unwrap().data;
        assert!(cached.unwrap().usage_stale);
        let usage = service.day_usage(day.to_string(), false).await.unwrap();
        assert!(usage.data.models.is_empty());
        aggregate.assert_hits(denied);
        service.day_usage(day.to_string(), true).await.unwrap();
        aggregate.assert_hits(denied + 1);
        task.abort();
        // Un cycle encore en cours peut écrire après coup : nettoyage au mieux.
        let _ = std::fs::remove_dir_all(directory);
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
        service.flush().await;
        task.abort();
        let (service, task) =
            start_with_storage(store.clone(), paths(), Arc::new(|_, _| Ok(false)));
        let cached = service.history(HistoryMode::Cached).await.unwrap().data;
        assert_eq!(cached, fresh);
        service.flush().await;
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
                    whole_proxy: false,
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
        service.flush().await;
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

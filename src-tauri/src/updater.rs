//! Coordinateur indépendant de LiteLLM : une seule opération, des effets injectables,
//! et des snapshots ordonnés pour le panneau, même lorsqu'il est masqué.
use std::{
    collections::BTreeSet,
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use shared::{UpdatePhase, UpdateStatus};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::{watch, Mutex};

const INTERVAL: Duration = Duration::from_secs(600);
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const NOTICE_ID: i32 = 100_001;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Une opération de mise à jour est déjà en cours")]
    Busy,
    #[error("Vérification de mise à jour impossible : {0}")]
    Check(String),
    #[error("Installation impossible : {0}")]
    Install(String),
    #[error("La version disponible a changé ; vérifiez puis relancez l'installation")]
    Obsolete,
    #[error("{0}")]
    Unavailable(String),
}

type Progress = Arc<dyn Fn(UpdatePhase, u64, Option<u64>) + Send + Sync>;
type Publish = Arc<dyn Fn(&UpdateStatus) + Send + Sync>;

/// Les effets système sont séparés pour tester les courses sans remplacer une vraie app.
pub(crate) trait Backend: Send + Sync + 'static {
    type Release: Send;
    fn version(release: &Self::Release) -> &str;
    fn check(&self) -> impl Future<Output = Result<Option<Self::Release>, Error>> + Send;
    fn install(
        &self,
        release: Self::Release,
        progress: Progress,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    fn notify(&self, version: &str) -> Result<(), String>;
    fn restart(&self);
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Notices {
    versions: BTreeSet<String>,
}

struct Inner<B> {
    backend: B,
    operation: Mutex<()>,
    install_requested: AtomicBool,
    notices: Mutex<Notices>,
    path: Option<PathBuf>,
    state: watch::Sender<UpdateStatus>,
    publish: Publish,
}

pub(crate) struct Service<B = TauriBackend>(Arc<Inner<B>>);

/// Libère la réservation même si une tâche d'installation est interrompue.
struct InstallRequest<'a>(&'a AtomicBool);
impl Drop for InstallRequest<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<B> Clone for Service<B> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<B: Backend> Service<B> {
    fn new(backend: B, version: String, path: Option<PathBuf>, publish: Publish) -> Self {
        let (state, _) = watch::channel(UpdateStatus {
            current_version: version,
            ..Default::default()
        });
        Self(Arc::new(Inner {
            backend,
            operation: Mutex::new(()),
            install_requested: AtomicBool::new(false),
            notices: Mutex::new(Notices::default()),
            path,
            state,
            publish,
        }))
    }

    pub fn status(&self) -> UpdateStatus {
        self.0.state.borrow().clone()
    }

    fn change(&self, change: impl FnOnce(&mut UpdateStatus)) {
        self.0.state.send_modify(|state| {
            change(state);
            state.revision += 1;
        });
        (self.0.publish)(&self.status());
    }

    fn failed(&self, error: &Error) {
        eprintln!("{error}");
        self.change(|s| {
            s.phase = UpdatePhase::Failed;
            s.error = Some(error.to_string());
        });
    }

    async fn load_notices(&self) {
        let Some(path) = self.0.path.clone() else {
            return;
        };
        match tauri::async_runtime::spawn_blocking(move || {
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        })
        .await
        {
            Ok(Some(notices)) => *self.0.notices.lock().await = notices,
            Ok(None) => {}
            Err(error) => eprintln!("Lecture des notifications de mise à jour : {error}"),
        }
    }

    async fn notify_once(&self, version: &str) {
        let mut notices = self.0.notices.lock().await;
        if notices.versions.contains(version) {
            return;
        }
        if let Err(error) = self.0.backend.notify(version) {
            eprintln!("Notification de mise à jour : {error}");
            return;
        }
        // Comme pour les budgets : un envoi accepté ne se répète pas dans cette session,
        // même si la persistance échoue. macOS peut toutefois masquer la notification.
        notices.versions.insert(version.to_owned());
        if let Some(path) = self.0.path.clone() {
            let saved = Notices {
                versions: notices.versions.clone(),
            };
            let result = tauri::async_runtime::spawn_blocking(move || {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(crate::persistence::Error::from)?;
                }
                crate::persistence::write(&path, &saved)
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                eprintln!("Persistance des notifications de mise à jour : {result:?}");
            }
        }
    }

    pub async fn check(&self, notify: bool) -> Result<UpdateStatus, Error> {
        if self.status().phase == UpdatePhase::Disabled
            || self.0.install_requested.load(Ordering::Acquire)
        {
            return Ok(self.status());
        }
        // Un clic pendant une vérification observe la même opération via les snapshots.
        let Ok(_operation) = self.0.operation.try_lock() else {
            return Ok(self.status());
        };
        self.change(|s| {
            s.phase = UpdatePhase::Checking;
            s.error = None;
        });
        match self.0.backend.check().await {
            Ok(release) => {
                let version = release.as_ref().map(|r| B::version(r).to_owned());
                self.change(|s| {
                    s.phase = if version.is_some() {
                        UpdatePhase::Available
                    } else {
                        UpdatePhase::UpToDate
                    };
                    s.available_version = version.clone();
                });
                if notify {
                    if let Some(version) = version {
                        self.notify_once(&version).await;
                    }
                }
                Ok(self.status())
            }
            Err(error) => {
                self.failed(&error);
                Err(error)
            }
        }
    }

    pub async fn install(&self, expected_version: &str) -> Result<(), Error> {
        if self.status().phase == UpdatePhase::Disabled {
            return Err(Error::Unavailable(self.status().error.unwrap_or_default()));
        }
        self.0
            .install_requested
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _request = InstallRequest(&self.0.install_requested);
        // Un clic sur la notification peut arriver pendant le contrôle périodique
        // ou la persistance de cette notification : attendre, plutôt que perdre le clic.
        let _operation = self.0.operation.lock().await;
        self.change(|s| {
            s.phase = UpdatePhase::Checking;
            s.error = None;
            s.downloaded_bytes = 0;
            s.total_bytes = None;
        });
        let result = async {
            let release = self.0.backend.check().await?;
            let version = release.as_ref().map(|r| B::version(r).to_owned());
            self.change(|s| s.available_version = version.clone());
            let release = release
                .filter(|_| version.as_deref() == Some(expected_version))
                .ok_or(Error::Obsolete)?;
            self.change(|s| s.phase = UpdatePhase::Downloading);
            let service = self.clone();
            let progress = Arc::new(move |phase, downloaded, total| {
                service.change(|s| {
                    s.phase = phase;
                    s.downloaded_bytes = downloaded;
                    s.total_bytes = total;
                });
            });
            self.0.backend.install(release, progress).await?;
            self.change(|s| s.phase = UpdatePhase::Restarting);
            self.0.backend.restart();
            Ok(())
        }
        .await;
        if let Err(error) = &result {
            self.failed(error);
        }
        result
    }

    async fn poll(&self) {
        let mut interval = tokio::time::interval(INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let _ = self.check(true).await;
        }
    }
}

pub(crate) struct TauriBackend {
    app: AppHandle,
}

/// Comparaison de la version embarquée, et non de CARGO_PKG_VERSION (non modifiée en CI).
fn newer_stable(current: &semver::Version, remote: &semver::Version) -> bool {
    remote.pre.is_empty() && remote.cmp_precedence(current).is_gt()
}

fn validate_artifact(update: &Update) -> Result<(), Error> {
    let url = &update.download_url;
    let prefix = format!(
        "/clementreiffers/litellm-ember/releases/download/{}/",
        update.version
    );
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.path().starts_with(&prefix)
        || !url.path().ends_with(".app.tar.gz")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(Error::Check(
            "L'archive ne provient pas de la release GitHub attendue".into(),
        ));
    }
    Ok(())
}

async fn check_app<R: Runtime>(app: &AppHandle<R>) -> Result<Option<Update>, Error> {
    app.updater_builder()
        .version_comparator(|current, remote| newer_stable(&current, &remote.version))
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|e| Error::Check(e.to_string()))?
        .check()
        .await
        .map_err(|e| Error::Check(e.to_string()))
}

/// Le bundle est déterminé à partir de l'exécutable, jamais du cwd ni d'un chemin du frontend.
fn bundle_path(executable: &std::path::Path, development: bool) -> Result<PathBuf, Error> {
    if development {
        return Err(Error::Unavailable(
            "Mise à jour désactivée en mode développement".into(),
        ));
    }
    if executable
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
    {
        return Err(Error::Unavailable("Déplacez Ember.app dans un dossier permanent puis relancez-la avant de la mettre à jour".into()));
    }
    let macos = executable
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "MacOS"));
    let contents = macos
        .and_then(|p| p.parent())
        .filter(|p| p.file_name().is_some_and(|n| n == "Contents"));
    contents
        .and_then(|p| p.parent())
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| {
            Error::Unavailable("L'installation nécessite une application Ember.app complète".into())
        })
}

impl Backend for TauriBackend {
    type Release = Update;
    fn version(release: &Update) -> &str {
        &release.version
    }
    async fn check(&self) -> Result<Option<Update>, Error> {
        let release = check_app(&self.app).await?;
        if let Some(update) = &release {
            validate_artifact(update)?;
        }
        Ok(release)
    }
    async fn install(&self, mut release: Update, progress: Progress) -> Result<(), Error> {
        let executable =
            tauri::utils::platform::current_exe().map_err(|e| Error::Install(e.to_string()))?;
        bundle_path(&executable, tauri::is_dev())?;
        release.timeout = Some(DOWNLOAD_TIMEOUT);
        let mut downloaded = 0_u64;
        let bytes = release
            .download(
                |chunk, total| {
                    downloaded = downloaded.saturating_add(chunk as u64);
                    progress(UpdatePhase::Downloading, downloaded, total);
                },
                || {},
            )
            .await
            .map_err(|e| Error::Install(e.to_string()))?;
        progress(UpdatePhase::Installing, downloaded, Some(downloaded));
        tauri::async_runtime::spawn_blocking(move || release.install(bytes)).await
            .map_err(|e| Error::Install(e.to_string()))?
            .map_err(|e| Error::Install(format!("{e}. Vérifiez les droits du dossier et qu'il ne s'agit pas d'un volume en lecture seule.")))
    }
    fn notify(&self, version: &str) -> Result<(), String> {
        self.app
            .notification()
            .builder()
            .id(NOTICE_ID)
            .title(format!("🔥 Ember {version} est disponible"))
            .body("Cliquez pour installer la mise à jour et redémarrer Ember.")
            .extra("kind", "ember-update")
            .extra("version", version)
            .show()
            .map_err(|e| e.to_string())
    }
    fn restart(&self) {
        self.app.restart();
    }
}

fn clicked_version(action: &tauri_plugin_notification::ActionPerformed) -> Option<&str> {
    if action.action_id() != "tap" {
        return None;
    }
    let notice = action.notification()?;
    if notice.id() != NOTICE_ID || notice.extra().get("kind")?.as_str()? != "ember-update" {
        return None;
    }
    notice.extra().get("version")?.as_str()
}

/// Installe les handlers avant le premier contrôle. Aucune permission updater n'est exposée au JS.
pub fn start(app: &AppHandle, path: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.clone();
    let service = Service::new(
        TauriBackend { app: app.clone() },
        // Tauri utilise la version GitHub injectée par semantic-release ; sans
        // surcharge de release, l'absence de config.version active le repli Cargo.
        app.package_info().version.to_string(),
        path,
        Arc::new(move |status| {
            let _ = handle.emit("update-status-changed", status);
        }),
    );
    let configured = app
        .config()
        .plugins
        .0
        .get("updater")
        .and_then(|c| c.get("pubkey"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|key| !key.trim().is_empty());
    if tauri::is_dev() || !configured {
        service.change(|s| {
            s.phase = UpdatePhase::Disabled;
            s.error = Some("Les mises à jour sont disponibles dans les versions officielles installées d'Ember".into());
        });
    }
    app.manage(service.clone());
    let handle = app.clone();
    app.notification().on_action(move |action| {
        let Some(version) = clicked_version(action).map(str::to_owned) else {
            return;
        };
        let app = handle.clone();
        tauri::async_runtime::spawn(async move {
            crate::tray::show_updates(&app);
            let _ = app.state::<Service>().install(&version).await;
        });
    })?;
    tauri::async_runtime::spawn(async move {
        // Le verrou empêche un clic manuel de dépasser le chargement de l'anti-doublon.
        {
            let _operation = service.0.operation.lock().await;
            service.load_notices().await;
        }
        if service.status().phase != UpdatePhase::Disabled {
            service.poll().await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Effects {
        checks: AtomicUsize,
        installs: AtomicUsize,
        restarts: AtomicUsize,
        notifications: AtomicUsize,
        fail_check: AtomicBool,
        fail_install: AtomicBool,
        fail_notify: AtomicBool,
        hold_check: AtomicBool,
        hold_install: AtomicBool,
        released: tokio::sync::Notify,
    }
    #[derive(Clone)]
    struct Fake(Arc<Effects>);
    impl Backend for Fake {
        type Release = String;
        fn version(release: &String) -> &str {
            release
        }
        async fn check(&self) -> Result<Option<String>, Error> {
            self.0.checks.fetch_add(1, Ordering::SeqCst);
            if self.0.hold_check.load(Ordering::SeqCst) {
                self.0.released.notified().await;
            }
            if self.0.fail_check.load(Ordering::SeqCst) {
                return Err(Error::Check("offline".into()));
            }
            Ok(Some("2.0.0".into()))
        }
        async fn install(&self, _: String, progress: Progress) -> Result<(), Error> {
            self.0.installs.fetch_add(1, Ordering::SeqCst);
            if self.0.hold_install.load(Ordering::SeqCst) {
                self.0.released.notified().await;
            }
            if self.0.fail_install.load(Ordering::SeqCst) {
                return Err(Error::Install("autorisation refusée".into()));
            }
            progress(UpdatePhase::Downloading, 50, Some(100));
            progress(UpdatePhase::Installing, 100, Some(100));
            Ok(())
        }
        fn notify(&self, _: &str) -> Result<(), String> {
            self.0.notifications.fetch_add(1, Ordering::SeqCst);
            if self.0.fail_notify.load(Ordering::SeqCst) {
                Err("refus".into())
            } else {
                Ok(())
            }
        }
        fn restart(&self) {
            self.0.restarts.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn service(path: Option<PathBuf>) -> (Service<Fake>, Arc<Effects>) {
        let effects = Arc::new(Effects::default());
        (
            Service::new(
                Fake(effects.clone()),
                "1.0.0".into(),
                path,
                Arc::new(|_| {}),
            ),
            effects,
        )
    }
    async fn wait_phase(service: &Service<Fake>, phase: UpdatePhase) {
        service
            .0
            .state
            .subscribe()
            .wait_for(|s| s.phase == phase)
            .await
            .unwrap();
    }

    #[test]
    fn versions_are_ordered_semantically_and_only_stable_upgrades_are_allowed() {
        let current = semver::Version::parse("1.9.0").unwrap();
        for (version, expected) in [
            ("1.10.0", true),
            ("2.0.0", true),
            ("1.9.0", false),
            ("1.8.9", false),
            ("2.0.0-beta.1", false),
        ] {
            assert_eq!(newer_stable(&current, &version.parse().unwrap()), expected);
        }
    }

    #[tokio::test]
    async fn duplicate_checks_share_the_running_operation() {
        let (service, effects) = service(None);
        effects.hold_check.store(true, Ordering::SeqCst);
        let other = service.clone();
        let task = tokio::spawn(async move { other.check(true).await });
        wait_phase(&service, UpdatePhase::Checking).await;
        assert_eq!(
            service.check(false).await.unwrap().phase,
            UpdatePhase::Checking
        );
        effects.released.notify_one();
        task.await.unwrap().unwrap();
        assert_eq!(effects.checks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_notification_click_waits_for_the_current_check_instead_of_being_lost() {
        let (service, effects) = service(None);
        effects.hold_check.store(true, Ordering::SeqCst);
        let checking = service.clone();
        let check = tokio::spawn(async move { checking.check(true).await });
        wait_phase(&service, UpdatePhase::Checking).await;
        let installing = service.clone();
        let install = tokio::spawn(async move { installing.install("2.0.0").await });
        while !service.0.install_requested.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert!(matches!(service.install("2.0.0").await, Err(Error::Busy)));
        assert_eq!(effects.installs.load(Ordering::SeqCst), 0);
        effects.hold_check.store(false, Ordering::SeqCst);
        effects.released.notify_one();
        check.await.unwrap().unwrap();
        install.await.unwrap().unwrap();
        assert_eq!(effects.checks.load(Ordering::SeqCst), 2);
        assert_eq!(effects.installs.load(Ordering::SeqCst), 1);
        assert_eq!(effects.restarts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn installs_once_and_restarts_only_after_success() {
        let (service, effects) = service(None);
        effects.hold_install.store(true, Ordering::SeqCst);
        let other = service.clone();
        let task = tokio::spawn(async move { other.install("2.0.0").await });
        wait_phase(&service, UpdatePhase::Downloading).await;
        assert!(matches!(service.install("2.0.0").await, Err(Error::Busy)));
        assert_eq!(
            service.check(true).await.unwrap().phase,
            UpdatePhase::Downloading
        );
        assert_eq!(effects.restarts.load(Ordering::SeqCst), 0);
        effects.released.notify_one();
        task.await.unwrap().unwrap();
        assert_eq!(effects.installs.load(Ordering::SeqCst), 1);
        assert_eq!(effects.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(service.status().phase, UpdatePhase::Restarting);
    }

    #[tokio::test]
    async fn refused_installation_and_network_failures_are_retryable_without_restarting() {
        let (service, effects) = service(None);
        effects.fail_check.store(true, Ordering::SeqCst);
        assert!(service.install("2.0.0").await.is_err());
        assert_eq!(effects.installs.load(Ordering::SeqCst), 0);
        effects.fail_check.store(false, Ordering::SeqCst);
        effects.fail_install.store(true, Ordering::SeqCst);
        assert!(service.install("2.0.0").await.is_err());
        assert_eq!(effects.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(service.status().phase, UpdatePhase::Failed);
        effects.fail_install.store(false, Ordering::SeqCst);
        service.install("2.0.0").await.unwrap();
        assert_eq!(effects.restarts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_obsolete_notification_never_installs_a_different_version() {
        let (service, effects) = service(None);
        assert!(matches!(
            service.install("1.5.0").await,
            Err(Error::Obsolete)
        ));
        assert_eq!(service.status().available_version.as_deref(), Some("2.0.0"));
        assert_eq!(effects.installs.load(Ordering::SeqCst), 0);
        assert_eq!(effects.restarts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn notifications_survive_restart_and_failed_delivery_is_retried() {
        let path =
            std::env::temp_dir().join(format!("ember-updates-{}.json", uuid::Uuid::new_v4()));
        let (first, effects) = service(Some(path.clone()));
        effects.fail_notify.store(true, Ordering::SeqCst);
        first.check(true).await.unwrap();
        assert!(!path.exists());
        effects.fail_notify.store(false, Ordering::SeqCst);
        first.check(true).await.unwrap();
        first.check(true).await.unwrap();
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 2);
        let (second, effects) = service(Some(path.clone()));
        second.load_notices().await;
        second.check(true).await.unwrap();
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn manual_checks_do_not_notify_and_corrupt_notice_state_is_tolerated() {
        let path =
            std::env::temp_dir().join(format!("ember-updates-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"broken").unwrap();
        let (service, effects) = service(Some(path.clone()));
        service.load_notices().await;
        service.check(false).await.unwrap();
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 0);
        service.check(true).await.unwrap();
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn checks_at_start_and_every_ten_minutes_without_catchup_bursts() {
        let (service, effects) = service(None);
        let task = tokio::spawn(async move { service.poll().await });
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(599)).await;
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 3);
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[test]
    fn only_update_notification_taps_trigger_installation() {
        let payload = |id, kind: &str, action| {
            serde_json::from_value(serde_json::json!({
            "actionId": action, "notification": { "id": id, "extra": { "kind": kind, "version": "2.0.0" } }
        })).unwrap()
        };
        assert_eq!(
            clicked_version(&payload(NOTICE_ID, "ember-update", "tap")),
            Some("2.0.0")
        );
        assert_eq!(clicked_version(&payload(0, "budget", "tap")), None);
        assert_eq!(
            clicked_version(&payload(NOTICE_ID, "ember-update", "dismiss")),
            None
        );
        assert_eq!(clicked_version(&payload(NOTICE_ID, "budget", "tap")), None);
    }

    #[test]
    fn install_path_uses_the_running_bundle_and_rejects_dev_or_translocated_apps() {
        let executable = std::path::Path::new("/Users/test/My Apps/Ember.app/Contents/MacOS/ember");
        assert_eq!(
            bundle_path(executable, false).unwrap(),
            PathBuf::from("/Users/test/My Apps/Ember.app")
        );
        assert!(bundle_path(executable, true).is_err());
        assert!(bundle_path(std::path::Path::new("/tmp/ember"), false).is_err());
        assert!(bundle_path(
            std::path::Path::new(
                "/private/var/AppTranslocation/id/d/Ember.app/Contents/MacOS/ember"
            ),
            false
        )
        .is_err());
    }

    fn mock_app(endpoint: &str) -> tauri::App<tauri::test::MockRuntime> {
        let mut context = tauri::test::mock_context(tauri::test::noop_assets());
        context.package_info_mut().version = "1.0.0".parse().unwrap();
        context.config_mut().plugins.0.insert(
            "updater".into(),
            serde_json::json!({
            "pubkey": include_str!("../tests/fixtures/updater/public.key").trim(), "requireSignedVersion": true, "endpoints": [endpoint],
            }),
        );
        tauri::test::mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap()
    }
    fn manifest(version: &str, url: &str) -> serde_json::Value {
        serde_json::json!({ "version": version, "url": url, "signature": include_str!("../tests/fixtures/updater/archive.txt.sig").trim() })
    }

    #[tokio::test]
    async fn http_checks_handle_versions_invalid_json_missing_platforms_and_server_failures() {
        for (body, code, expected) in [
            (manifest("1.1.0", "https://github.com/clementreiffers/litellm-ember/releases/download/1.1.0/Ember.app.tar.gz"), 200, "available"),
            (manifest("1.0.0", "https://example.com/unused"), 200, "current"),
            (manifest("0.9.0", "https://example.com/unused"), 200, "current"),
            (manifest("2.0.0-beta.1", "https://example.com/unused"), 200, "current"),
            (serde_json::json!({ "version": "1.1.0", "platforms": {} }), 200, "error"),
            (serde_json::json!({ "version": "nonsense" }), 200, "error"),
            (serde_json::json!("not a manifest"), 200, "error"),
            (serde_json::json!({}), 503, "error"),
            (serde_json::json!({}), 404, "error"),
        ] {
            let server = MockServer::start();
            let request = server.mock(|when, then| { when.path("/latest.json"); then.status(code).json_body(body); });
            let app = mock_app(&format!("{}/latest.json", server.base_url()));
            let result = check_app(app.handle()).await;
            match expected {
                "available" => validate_artifact(&result.unwrap().unwrap()).unwrap(),
                "current" => assert!(result.unwrap().is_none()),
                _ => assert!(result.is_err()),
            }
            request.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn download_rejects_invalid_signatures_and_network_failure_before_installing() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/latest.json");
            then.json_body(manifest("1.1.0", &format!("{}/update", server.base_url())));
        });
        let mut download = server.mock(|when, then| {
            when.path("/update");
            then.body("unauthenticated archive");
        });
        let app = mock_app(&format!("{}/latest.json", server.base_url()));
        let release = check_app(app.handle()).await.unwrap().unwrap();
        assert!(validate_artifact(&release).is_err()); // Untrusted hosts cannot enter the production install path.
        assert!(release.download(|_, _| {}, || {}).await.is_err());
        download.assert_hits(1);
        download.delete();
        let download = server.mock(|when, then| {
            when.path("/update");
            then.status(503);
        });
        assert!(release.download(|_, _| {}, || {}).await.is_err());
        download.assert_hits(1);
    }

    #[tokio::test]
    async fn stalled_http_requests_are_bounded() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/latest.json");
            then.delay(Duration::from_millis(200))
                .json_body(manifest("1.1.0", "https://example.com/file"));
        });
        let app = mock_app(&format!("{}/latest.json", server.base_url()));
        let updater = app
            .updater_builder()
            .timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        assert!(updater.check().await.is_err());
    }
}

#[cfg(test)]
mod signed_artifact_tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn real_signature_accepts_exact_bytes_and_rejects_version_substitution() {
        let server = MockServer::start();
        let mut context = tauri::test::mock_context(tauri::test::noop_assets());
        context.config_mut().plugins.0.insert(
            "updater".into(),
            serde_json::json!({
                "pubkey": include_str!("../tests/fixtures/updater/public.key").trim(),
                "requireSignedVersion": true,
                "endpoints": [format!("{}/latest.json", server.base_url())],
            }),
        );
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap();
        let payload = include_bytes!("../tests/fixtures/updater/archive.txt");
        let mut download = server.mock(|when, then| {
            when.path("/artifact");
            then.body(payload);
        });
        for (version, valid) in [("1.1.0", true), ("9.9.9", false)] {
            let mut manifest = server.mock(|when, then| {
                when.path("/latest.json");
                then.json_body(serde_json::json!({
                    "version": version, "url": format!("{}/artifact", server.base_url()),
                    "signature": include_str!("../tests/fixtures/updater/archive.txt.sig").trim(),
                }));
            });
            let update = check_app(app.handle()).await.unwrap().unwrap();
            let result = update.download(|_, _| {}, || {}).await;
            assert_eq!(result.is_ok(), valid);
            if let Ok(bytes) = result {
                assert_eq!(&bytes, payload);
            }
            manifest.delete();
        }
        download.delete();
        server.mock(|when, then| {
            when.path("/artifact");
            then.delay(Duration::from_millis(200)).body(payload);
        });
        server.mock(|when, then| {
            when.path("/latest.json");
            then.json_body(serde_json::json!({
                "version": "1.1.0", "url": format!("{}/artifact", server.base_url()),
                "signature": include_str!("../tests/fixtures/updater/archive.txt.sig").trim(),
            }));
        });
        let mut update = check_app(app.handle()).await.unwrap().unwrap();
        update.timeout = Some(Duration::from_millis(10));
        assert!(update.download(|_, _| {}, || {}).await.is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod bundle_tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn signed_bundle_replaces_only_the_selected_app_at_its_existing_custom_path() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory
            .path()
            .join("My Apps with spaces/Ember Custom.app");
        let executable = bundle.join("Contents/MacOS/ember");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "old binary").unwrap();
        let settings = directory.path().join("settings.json");
        std::fs::write(&settings, "untouched settings").unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/latest.json");
            then.json_body(serde_json::json!({
                "version": "1.1.0", "url": format!("{}/artifact", server.base_url()),
                "signature": include_str!("../tests/fixtures/updater/Ember.app.tar.gz.sig").trim(),
            }));
        });
        server.mock(|when, then| {
            when.path("/artifact");
            then.body(include_bytes!("../tests/fixtures/updater/Ember.app.tar.gz"));
        });
        let mut context = tauri::test::mock_context(tauri::test::noop_assets());
        context.config_mut().plugins.0.insert("updater".into(), serde_json::json!({
            "pubkey": include_str!("../tests/fixtures/updater/public.key").trim(), "requireSignedVersion": true,
            "endpoints": [format!("{}/latest.json", server.base_url())],
        }));
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap();
        let update = app
            .updater_builder()
            .executable_path(&executable)
            .build()
            .unwrap()
            .check()
            .await
            .unwrap()
            .unwrap();
        let bytes = update.download(|_, _| {}, || {}).await.unwrap();
        tokio::task::spawn_blocking(move || update.install(bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(executable).unwrap(),
            "signed test binary - never executed\n"
        );
        assert!(std::fs::read_to_string(bundle.join("Contents/Info.plist"))
            .unwrap()
            .contains("1.1.0"));
        assert_eq!(
            std::fs::read_to_string(settings).unwrap(),
            "untouched settings"
        );
        assert!(!directory.path().join("Ember.app").exists());
    }
}

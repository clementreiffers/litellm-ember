//! Coordinateur indépendant de LiteLLM : une seule opération, des effets injectables,
//! et des snapshots ordonnés pour le panneau, même lorsqu'il est masqué.
use std::{collections::BTreeSet, future::Future, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use shared::{UpdatePhase, UpdateStatus};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::{watch, Mutex};

const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const NOTICE_ID: i32 = 100_001;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Vérification de mise à jour impossible : {0}")]
    Check(String),
    #[error("Mise à jour impossible : {0}")]
    Install(String),
    #[error("Redémarrage impossible : {0}")]
    Restart(String),
}

type Progress = Arc<dyn Fn(UpdatePhase, u64, Option<u64>) + Send + Sync>;

type Publish = Arc<dyn Fn(&UpdateStatus) + Send + Sync>;

/// Les effets système sont séparés pour tester les courses sans remplacer une vraie app.
pub(crate) trait Backend: Send + Sync + 'static {
    type Release: Send;
    fn version(release: &Self::Release) -> &str;
    fn check(&self) -> impl Future<Output = Result<Option<Self::Release>, Error>> + Send;
    fn notify(&self, version: &str) -> Result<(), String>;
    fn install(
        &self,
        release: Self::Release,
        progress: Progress,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    fn restart(&self) -> Result<(), Error>;
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Notices {
    versions: BTreeSet<String>,
}

struct Inner<B: Backend> {
    backend: B,
    operation: Mutex<()>,
    notices: Mutex<Notices>,
    release: Mutex<Option<B::Release>>,
    path: Option<PathBuf>,
    state: watch::Sender<UpdateStatus>,
    publish: Publish,
}

pub(crate) struct Service<B: Backend = TauriBackend>(Arc<Inner<B>>);

impl<B: Backend> Clone for Service<B> {
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
            notices: Mutex::new(Notices::default()),
            release: Mutex::new(None),
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
            s.failed_phase = Some(s.phase);
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
        // Un clic pendant une vérification observe la même opération via les snapshots.
        let Ok(_operation) = self.0.operation.try_lock() else {
            return Ok(self.status());
        };
        if matches!(
            self.status().phase,
            UpdatePhase::ReadyToRestart | UpdatePhase::Restarting
        ) {
            return Ok(self.status());
        }
        self.change(|s| {
            s.failed_phase = None;
            s.downloaded_bytes = 0;
            s.total_bytes = None;
            s.phase = UpdatePhase::Checking;
            s.error = None;
        });
        match self.0.backend.check().await {
            Ok(release) => {
                let version = release.as_ref().map(|r| B::version(r).to_owned());
                *self.0.release.lock().await = release;
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

    pub async fn install(&self, expected_version: &str) -> Result<UpdateStatus, Error> {
        let Ok(_operation) = self.0.operation.try_lock() else {
            return Ok(self.status());
        };
        let status = self.status();
        if matches!(
            status.phase,
            UpdatePhase::ReadyToRestart | UpdatePhase::Restarting
        ) {
            return Ok(status);
        }
        if status.available_version.as_deref() != Some(expected_version) {
            return Err(Error::Install(
                "La version proposée a changé. Vérifiez les mises à jour.".into(),
            ));
        }
        self.change(|s| {
            s.phase = UpdatePhase::Downloading;
            s.error = None;
            s.failed_phase = None;
            s.downloaded_bytes = 0;
            s.total_bytes = None;
        });
        let result = async {
            // Le descripteur reste privé. Après un échec, revérifier le manifeste avant de réessayer.
            let release = match self.0.release.lock().await.take() {
                Some(release) => Some(release),
                None => self.0.backend.check().await?,
            };
            let release = release.ok_or_else(|| {
                Error::Install(
                    "Cette mise à jour n’est plus disponible. Vérifiez à nouveau.".into(),
                )
            })?;
            if B::version(&release) != expected_version {
                return Err(Error::Install(
                    "La version proposée a changé. Vérifiez à nouveau.".into(),
                ));
            }
            let service = self.clone();
            self.0
                .backend
                .install(
                    release,
                    Arc::new(move |phase, bytes, total| {
                        service.change(|s| {
                            s.phase = phase;
                            if phase != UpdatePhase::Verifying {
                                s.downloaded_bytes = bytes;
                                s.total_bytes = total;
                            }
                        });
                    }),
                )
                .await
        }
        .await;
        match result {
            Ok(()) => {
                self.change(|s| s.phase = UpdatePhase::ReadyToRestart);
                Ok(self.status())
            }
            Err(error) => {
                self.failed(&error);
                Err(error)
            }
        }
    }

    pub async fn restart(&self) -> Result<(), Error> {
        let _operation = self
            .0
            .operation
            .try_lock()
            .map_err(|_| Error::Restart("Une opération est encore en cours.".into()))?;
        if self.status().phase != UpdatePhase::ReadyToRestart {
            return Err(Error::Restart(
                "Aucune mise à jour installée en attente.".into(),
            ));
        }
        self.change(|s| {
            s.phase = UpdatePhase::Restarting;
            s.error = None;
        });
        if let Err(error) = self.0.backend.restart() {
            self.change(|s| {
                s.phase = UpdatePhase::ReadyToRestart;
                s.error = Some(error.to_string());
            });
            return Err(error);
        }
        Ok(())
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

fn release_url(version: &str) -> Result<String, String> {
    semver::Version::parse(version.strip_prefix('v').unwrap_or(version))
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "https://github.com/clementreiffers/litellm-ember/releases/tag/{version}"
    ))
}

pub async fn open_release(version: &str) -> Result<(), String> {
    let url = release_url(version)?;
    tauri::async_runtime::spawn_blocking(move || {
        let status = std::process::Command::new("/usr/bin/open")
            .arg(url)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err("Impossible d’ouvrir la release GitHub".into())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

fn configured_updater(
    builder: tauri_plugin_updater::UpdaterBuilder,
) -> Result<tauri_plugin_updater::Updater, Error> {
    builder
        .version_comparator(|current, remote| newer_stable(&current, &remote.version))
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|e| Error::Check(e.to_string()))
}

impl Backend for TauriBackend {
    type Release = Update;
    fn version(release: &Update) -> &str {
        &release.version
    }
    async fn check(&self) -> Result<Option<Update>, Error> {
        let updater = configured_updater(self.app.updater_builder())?;
        updater
            .check()
            .await
            .map_err(|e| Error::Check(e.to_string()))
    }
    async fn install(&self, mut release: Update, progress: Progress) -> Result<(), Error> {
        let executable = tauri::process::current_binary(&self.app.env())
            .map_err(|e| Error::Install(e.to_string()))?;
        validate_install_path(&executable)?;
        release.timeout = Some(DOWNLOAD_TIMEOUT);
        let mut downloaded = 0;
        let mut total_bytes = None;
        let verifying = progress.clone();
        let bytes = release
            .download(
                |count, total| {
                    downloaded += count as u64;
                    total_bytes = total;
                    progress(UpdatePhase::Downloading, downloaded, total);
                },
                || verifying(UpdatePhase::Verifying, 0, None),
            )
            .await
            .map_err(|e| Error::Install(e.to_string()))?;
        progress(UpdatePhase::Installing, downloaded, total_bytes);
        tauri::async_runtime::spawn_blocking(move || release.install(bytes))
            .await
            .map_err(|e| Error::Install(e.to_string()))?
            .map_err(|e| Error::Install(e.to_string()))
    }
    fn restart(&self) -> Result<(), Error> {
        self.app.request_restart();
        Ok(())
    }
    fn notify(&self, version: &str) -> Result<(), String> {
        self.app
            .notification()
            .builder()
            .id(NOTICE_ID)
            .title(format!("🔥 Ember {version} est disponible"))
            .body("Ouvrez Ember pour installer la mise à jour.")
            .extra("kind", "ember-update")
            .extra("version", version)
            .show()
            .map_err(|e| e.to_string())
    }
}

/// Ne pas remplacer une copie transloquée ou montée depuis le disque d’installation.
fn validate_install_path(executable: &std::path::Path) -> Result<(), Error> {
    let bundle = executable
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    if executable
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
        || executable.starts_with("/Volumes")
        || bundle.is_none_or(|p| p.extension().is_none_or(|e| e != "app"))
    {
        return Err(Error::Install(
            "Déplacez Ember dans Applications, puis relancez-la avant la mise à jour.".into(),
        ));
    }
    Ok(())
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
    app.manage(service.clone());
    let handle = app.clone();
    app.notification().on_action(move |action| {
        let Some(_) = clicked_version(action) else {
            return;
        };
        let app = handle.clone();
        tauri::async_runtime::spawn(async move {
            crate::tray::show_updates(&app);
        });
    })?;
    tauri::async_runtime::spawn(async move {
        // Le verrou empêche un clic manuel de dépasser le chargement de l'anti-doublon.
        {
            let _operation = service.0.operation.lock().await;
            service.load_notices().await;
        }
        service.poll().await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Effects {
        checks: AtomicUsize,
        installs: AtomicUsize,
        restarts: AtomicUsize,
        fail_install: AtomicBool,
        hold_install: AtomicBool,
        fail_restart: AtomicBool,
        notifications: AtomicUsize,
        fail_check: AtomicBool,
        fail_notify: AtomicBool,
        hold_check: AtomicBool,
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
            progress(UpdatePhase::Downloading, 50, Some(100));
            if self.0.hold_install.load(Ordering::SeqCst) {
                self.0.released.notified().await;
            }
            progress(UpdatePhase::Verifying, 0, None);
            if self.0.fail_install.load(Ordering::SeqCst) {
                return Err(Error::Install("signature invalide".into()));
            }
            progress(UpdatePhase::Installing, 100, Some(100));
            Ok(())
        }
        fn restart(&self) -> Result<(), Error> {
            self.0.restarts.fetch_add(1, Ordering::SeqCst);
            if self.0.fail_restart.load(Ordering::SeqCst) {
                Err(Error::Restart("refus".into()))
            } else {
                Ok(())
            }
        }
        fn notify(&self, _: &str) -> Result<(), String> {
            self.0.notifications.fetch_add(1, Ordering::SeqCst);
            if self.0.fail_notify.load(Ordering::SeqCst) {
                Err("refus".into())
            } else {
                Ok(())
            }
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

    #[tokio::test]
    async fn installation_is_exclusive_and_ready_state_survives_checks() {
        let (service, effects) = service(None);
        assert!(service.restart().await.is_err());
        assert!(service.install("2.0.0").await.is_err());
        service.check(false).await.unwrap();
        assert!(service.install("3.0.0").await.is_err());
        effects.hold_install.store(true, Ordering::SeqCst);
        let other = service.clone();
        let task = tokio::spawn(async move { other.install("2.0.0").await });
        wait_phase(&service, UpdatePhase::Downloading).await;
        assert_eq!(service.status().downloaded_bytes, 50);
        assert!(service.restart().await.is_err());
        assert_eq!(
            service.check(false).await.unwrap().phase,
            UpdatePhase::Downloading
        );
        assert_eq!(
            service.install("2.0.0").await.unwrap().phase,
            UpdatePhase::Downloading
        );
        effects.released.notify_one();
        assert_eq!(
            task.await.unwrap().unwrap().phase,
            UpdatePhase::ReadyToRestart
        );
        assert_eq!(
            service.check(true).await.unwrap().phase,
            UpdatePhase::ReadyToRestart
        );
        assert_eq!(
            service.install("2.0.0").await.unwrap().phase,
            UpdatePhase::ReadyToRestart
        );
        assert_eq!(effects.checks.load(Ordering::SeqCst), 1);
        assert_eq!(effects.installs.load(Ordering::SeqCst), 1);
        effects.fail_restart.store(true, Ordering::SeqCst);
        assert!(service.restart().await.is_err());
        assert_eq!(service.status().phase, UpdatePhase::ReadyToRestart);
        effects.fail_restart.store(false, Ordering::SeqCst);
        service.restart().await.unwrap();
        assert_eq!(service.status().phase, UpdatePhase::Restarting);
    }

    #[tokio::test]
    async fn failed_verification_keeps_version_and_can_be_retried() {
        let (service, effects) = service(None);
        service.check(false).await.unwrap();
        effects.fail_install.store(true, Ordering::SeqCst);
        assert!(service.install("2.0.0").await.is_err());
        let status = service.status();
        assert_eq!(status.phase, UpdatePhase::Failed);
        assert_eq!(status.failed_phase, Some(UpdatePhase::Verifying));
        assert_eq!(status.available_version.as_deref(), Some("2.0.0"));
        assert!(service.restart().await.is_err());
        effects.fail_install.store(false, Ordering::SeqCst);
        assert_eq!(
            service.install("2.0.0").await.unwrap().phase,
            UpdatePhase::ReadyToRestart
        );
        assert_eq!(effects.checks.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn installation_requires_an_installed_bundle() {
        for path in [
            "/Volumes/Ember/Ember.app/Contents/MacOS/ember",
            "/private/var/AppTranslocation/id/d/Ember.app/Contents/MacOS/ember",
            "/tmp/ember",
        ] {
            assert!(validate_install_path(std::path::Path::new(path)).is_err());
        }
        assert!(validate_install_path(std::path::Path::new(
            "/Applications/Ember.app/Contents/MacOS/ember"
        ))
        .is_ok());
        assert!(validate_install_path(std::path::Path::new(
            "/Users/test/Applications/Ember.app/Contents/MacOS/ember"
        ))
        .is_ok());
    }

    #[test]
    fn release_links_only_open_our_version_tags() {
        assert_eq!(
            release_url("1.2.3").unwrap(),
            "https://github.com/clementreiffers/litellm-ember/releases/tag/1.2.3"
        );
        for invalid in ["https://example.com", "../../other", "1.2.3?redirect=bad"] {
            assert!(release_url(invalid).is_err());
        }
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
    async fn failed_checks_can_be_retried_without_losing_notification_state() {
        let (service, effects) = service(None);
        effects.fail_check.store(true, Ordering::SeqCst);
        assert!(service.check(true).await.is_err());
        assert_eq!(service.status().phase, UpdatePhase::Failed);
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 0);
        effects.fail_check.store(false, Ordering::SeqCst);
        assert_eq!(
            service.check(true).await.unwrap().phase,
            UpdatePhase::Available
        );
        assert!(service.status().error.is_none());
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 1);
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
    async fn checks_at_start_and_every_day_without_catchup_bursts() {
        let (service, effects) = service(None);
        let task = tokio::spawn(async move { service.poll().await });
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(86_399)).await;
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_secs(3 * 86_400)).await;
        tokio::task::yield_now().await;
        assert_eq!(effects.checks.load(Ordering::SeqCst), 3);
        assert_eq!(effects.notifications.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[test]
    fn only_update_notification_taps_open_the_panel() {
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
}

#[cfg(test)]
mod manifest_tests {
    use super::*;
    use httpmock::prelude::*;

    const KEY: &str = include_str!("../test-data/updater/public-key.txt");
    const SIGNATURE: &str = include_str!("../test-data/updater/payload.txt.sig");
    const PAYLOAD: &[u8] = include_bytes!("../test-data/updater/payload.txt");

    fn updater(server: &MockServer, target: &str) -> tauri_plugin_updater::Updater {
        let mut context = tauri::test::mock_context(tauri::test::noop_assets());
        context.package_info_mut().version = "1.9.0".parse().unwrap();
        context.config_mut().plugins.0.insert(
            "updater".into(),
            serde_json::json!({
                "pubkey": KEY.trim(), "requireSignedVersion": true,
                "dangerousInsecureTransportProtocol": true,
                "endpoints": [format!("{}/latest.json", server.base_url())]
            }),
        );
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap();
        configured_updater(app.updater_builder().target(target)).unwrap()
    }

    fn manifest(server: &MockServer, version: &str) -> serde_json::Value {
        let platform = serde_json::json!({
            "url": format!("{}/update", server.base_url()), "signature": SIGNATURE.trim()
        });
        serde_json::json!({"version": version, "platforms": {
            "darwin-aarch64": platform, "darwin-x86_64": platform
        }})
    }

    #[tokio::test]
    async fn manifest_checks_only_offer_newer_stable_versions_on_both_architectures() {
        for target in ["darwin-aarch64", "darwin-x86_64"] {
            for (version, available) in [
                ("1.10.0", true),
                ("1.9.0", false),
                ("1.8.0", false),
                ("2.0.0-beta.1", false),
            ] {
                let server = MockServer::start();
                let request = server.mock(|when, then| {
                    when.path("/latest.json").matches(|request| {
                        !request.headers.as_ref().is_some_and(|headers| {
                            headers
                                .iter()
                                .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                        })
                    });
                    then.json_body(manifest(&server, version));
                });
                assert_eq!(
                    updater(&server, target).check().await.unwrap().is_some(),
                    available
                );
                request.assert_hits(1);
            }
        }
    }

    #[tokio::test]
    async fn missing_manifest_http_errors_and_invalid_json_are_not_up_to_date() {
        for status in [404, 403, 429, 503, 200] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/latest.json");
                then.status(status).body("invalid");
            });
            assert!(updater(&server, "darwin-aarch64").check().await.is_err());
        }
    }

    #[tokio::test]
    async fn download_accepts_cli_signature_and_rejects_corruption_and_version_substitution() {
        for (version, bytes, valid) in [
            ("2.0.0", PAYLOAD, true),
            ("2.0.0", &b"corrupt"[..], false),
            ("3.0.0", PAYLOAD, false),
        ] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/latest.json");
                then.json_body(manifest(&server, version));
            });
            server.mock(|when, then| {
                when.path("/update");
                then.body(bytes);
            });
            let release = updater(&server, "darwin-aarch64")
                .check()
                .await
                .unwrap()
                .unwrap();
            let mut received = 0;
            let result = release.download(|count, _| received += count, || {}).await;
            assert_eq!(received, bytes.len());
            assert_eq!(result.is_ok(), valid);
            if valid {
                assert_eq!(result.unwrap(), PAYLOAD);
            }
        }
    }
}

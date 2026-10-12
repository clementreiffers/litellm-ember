//! Coordinateur indépendant de LiteLLM : une seule opération, des effets injectables,
//! et des snapshots ordonnés pour le panneau, même lorsqu'il est masqué.
use std::{collections::BTreeSet, future::Future, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use shared::{UpdatePhase, UpdateStatus};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::{watch, Mutex};

const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const NOTICE_ID: i32 = 100_001;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Vérification de mise à jour impossible : {0}")]
    Check(String),
}

type Publish = Arc<dyn Fn(&UpdateStatus) + Send + Sync>;

/// Les effets système sont séparés pour tester les courses sans remplacer une vraie app.
pub(crate) trait Backend: Send + Sync + 'static {
    type Release: Send;
    fn version(release: &Self::Release) -> &str;
    fn check(&self) -> impl Future<Output = Result<Option<Self::Release>, Error>> + Send;
    fn notify(&self, version: &str) -> Result<(), String>;
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Notices {
    versions: BTreeSet<String>,
}

struct Inner<B> {
    backend: B,
    operation: Mutex<()>,
    notices: Mutex<Notices>,
    path: Option<PathBuf>,
    state: watch::Sender<UpdateStatus>,
    publish: Publish,
}

pub(crate) struct Service<B = TauriBackend>(Arc<Inner<B>>);

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
    client: reqwest::Client,
}

/// Comparaison de la version embarquée, et non de CARGO_PKG_VERSION (non modifiée en CI).
fn newer_stable(current: &semver::Version, remote: &semver::Version) -> bool {
    remote.pre.is_empty() && remote.cmp_precedence(current).is_gt()
}

const ENDPOINT: &str = "https://api.github.com/repos/clementreiffers/litellm-ember/releases/latest";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

async fn check_release(
    client: &reqwest::Client,
    endpoint: &str,
    current: &semver::Version,
) -> Result<Option<String>, Error> {
    let result = async {
        let response = client
            .get(endpoint)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let release: Release = response.error_for_status()?.json().await?;
        Ok::<_, reqwest::Error>(Some(release))
    }
    .await
    .map_err(|e| Error::Check(e.to_string()))?;
    let Some(release) = result else {
        return Ok(None);
    };
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = semver::Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .map_err(|e| Error::Check(e.to_string()))?;
    Ok(newer_stable(current, &version).then_some(release.tag_name))
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

impl Backend for TauriBackend {
    type Release = String;
    fn version(release: &String) -> &str {
        release
    }
    async fn check(&self) -> Result<Option<String>, Error> {
        check_release(&self.client, ENDPOINT, &self.app.package_info().version).await
    }
    fn notify(&self, version: &str) -> Result<(), String> {
        self.app
            .notification()
            .builder()
            .id(NOTICE_ID)
            .title(format!("🔥 Ember {version} est disponible"))
            .body("Cliquez pour télécharger la mise à jour sur GitHub.")
            .extra("kind", "ember-update")
            .extra("version", version)
            .show()
            .map_err(|e| e.to_string())
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
        TauriBackend {
            app: app.clone(),
            client: reqwest::Client::builder()
                .user_agent("Ember")
                .timeout(CHECK_TIMEOUT)
                .build()?,
        },
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
        let Some(version) = clicked_version(action).map(str::to_owned) else {
            return;
        };
        let app = handle.clone();
        tauri::async_runtime::spawn(async move {
            crate::tray::show_updates(&app);
            if let Err(error) = open_release(&version).await {
                app.state::<Service>().change(|s| s.error = Some(error));
            }
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
    fn only_update_notification_taps_open_the_release() {
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
mod github_tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn public_checks_handle_versions_missing_releases_and_http_errors() {
        let client = reqwest::Client::builder()
            .user_agent("Ember")
            .timeout(CHECK_TIMEOUT)
            .build()
            .unwrap();
        for (tag, draft, prerelease, code, expected) in [
            ("1.10.0", false, false, 200, "available"),
            ("v2.0.0", false, false, 200, "available"),
            ("1.9.0", false, false, 200, "none"),
            ("1.8.0", false, false, 200, "none"),
            ("2.0.0-beta.1", false, false, 200, "none"),
            ("2.0.0", true, false, 200, "none"),
            ("2.0.0", false, true, 200, "none"),
            ("invalid", false, false, 200, "error"),
            ("", false, false, 404, "none"),
            ("", false, false, 403, "error"),
            ("", false, false, 429, "error"),
            ("", false, false, 503, "error"),
        ] {
            let server = MockServer::start();
            let request = server.mock(|when, then| {
                when.path("/latest")
                    .header("user-agent", "Ember")
                    .matches(|request| {
                        !request.headers.as_ref().is_some_and(|headers| {
                            headers
                                .iter()
                                .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                        })
                    });
                then.status(code).json_body(
                    serde_json::json!({"tag_name": tag, "draft": draft, "prerelease": prerelease}),
                );
            });
            let result = check_release(
                &client,
                &format!("{}/latest", server.base_url()),
                &"1.9.0".parse().unwrap(),
            )
            .await;
            match expected {
                "available" => assert_eq!(result.unwrap().as_deref(), Some(tag)),
                "none" => assert!(result.unwrap().is_none()),
                _ => assert!(result.is_err()),
            }
            request.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn invalid_json_and_stalled_requests_fail() {
        for delay in [0, 200] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.path("/latest");
                then.delay(Duration::from_millis(delay)).body("invalid");
            });
            let client = reqwest::Client::builder()
                .timeout(Duration::from_millis(20))
                .build()
                .unwrap();
            assert!(check_release(
                &client,
                &format!("{}/latest", server.base_url()),
                &"1.0.0".parse().unwrap()
            )
            .await
            .is_err());
        }
    }

    #[test]
    fn release_links_only_open_version_tags_on_our_repository() {
        assert_eq!(
            release_url("v1.2.3").unwrap(),
            "https://github.com/clementreiffers/litellm-ember/releases/tag/v1.2.3"
        );
        for invalid in ["https://example.com", "../../other", "1.2.3?redirect=bad"] {
            assert!(release_url(invalid).is_err());
        }
    }
}

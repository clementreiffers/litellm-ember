//! Le service continue à publier quand les paramètres sont fermés.
use leptos::prelude::*;
use shared::{UpdatePhase, UpdateStatus};

use crate::tauri;

pub fn description(status: &UpdateStatus) -> String {
    match status.phase {
        UpdatePhase::Idle => "Vérification automatique au démarrage et toutes les 24 heures".into(),
        UpdatePhase::Checking => "Recherche de mises à jour…".into(),
        UpdatePhase::UpToDate => "Ember est à jour".into(),
        UpdatePhase::Available => format!(
            "Ember {} est disponible",
            status.available_version.as_deref().unwrap_or("")
        ),
        UpdatePhase::Downloading => match status.total_bytes.filter(|total| *total > 0) {
            Some(total) => format!(
                "Téléchargement… {} %",
                status
                    .downloaded_bytes
                    .saturating_mul(100)
                    .checked_div(total)
                    .unwrap_or(0)
                    .min(100)
            ),
            None => format!("Téléchargement… {} Ko", status.downloaded_bytes / 1024),
        },
        UpdatePhase::Verifying => "Vérification de la signature…".into(),
        UpdatePhase::Installing => "Installation de la mise à jour…".into(),
        UpdatePhase::ReadyToRestart => format!(
            "Ember {} est installée. Redémarrez pour l’utiliser.",
            status.available_version.as_deref().unwrap_or("")
        ),
        UpdatePhase::Restarting => "Redémarrage d’Ember…".into(),
        UpdatePhase::Failed => "L’opération n’a pas abouti. Vous pouvez réessayer.".into(),
    }
}

pub fn accept(status: RwSignal<UpdateStatus>, incoming: UpdateStatus) {
    status.try_update(|current| {
        if incoming.revision >= current.revision {
            *current = incoming;
        }
    });
}

fn action_label(phase: UpdatePhase) -> &'static str {
    match phase {
        UpdatePhase::ReadyToRestart => "Redémarrer",
        UpdatePhase::Failed => "Réessayer la mise à jour",
        _ => "Mettre à jour",
    }
}

#[component]
pub fn Updates(status: RwSignal<UpdateStatus>) -> impl IntoView {
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<(u64, String)>);
    let disabled = move || pending.get() || status.with(|s| s.phase.busy());
    let check = move |_| {
        if disabled() {
            return;
        }
        pending.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            match tauri::check_for_updates().await {
                Ok(value) => accept(status, value),
                Err(message) => {
                    error.try_set(Some((status.with_untracked(|s| s.revision), message)));
                }
            }
            pending.try_set(false);
        });
    };
    let install = move |_| {
        if disabled() {
            return;
        }
        let snapshot = status.get_untracked();
        pending.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let result = if snapshot.phase == UpdatePhase::ReadyToRestart {
                tauri::restart_after_update().await
            } else if let Some(version) = snapshot.available_version {
                tauri::install_update(version)
                    .await
                    .map(|value| accept(status, value))
            } else {
                Ok(())
            };
            if let Err(message) = result {
                error.try_set(Some((status.with_untracked(|s| s.revision), message)));
            }
            pending.try_set(false);
        });
    };
    let open = move |_| {
        if disabled() {
            return;
        }
        let Some(version) = status.with(|s| s.available_version.clone()) else {
            return;
        };
        pending.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            if let Err(message) = tauri::open_update_release(version).await {
                error.try_set(Some((status.with_untracked(|s| s.revision), message)));
            }
            pending.try_set(false);
        });
    };
    view! {
        <section class="card form update-card" aria-label="Mises à jour">
            <h3>"Mises à jour"</h3>
            <div class="hint">{move || format!("Version installée : {}", status.with(|s| s.current_version.clone()))}</div>
            <div role="status" aria-live="polite">{move || status.with(description)}</div>
            {move || error.get().filter(|(revision, _)| *revision == status.with(|s| s.revision))
                .map(|(_, message)| message).or_else(|| status.with(|s| s.error.clone())).map(|text| view! {
                <div class="error" role="alert">{text}</div>
            })}
            <div class="actions">
                <button disabled=move || disabled() || status.with(|s| s.phase == UpdatePhase::ReadyToRestart) on:click=check>"Vérifier les mises à jour"</button>
                <Show when=move || status.with(|s| s.available_version.is_some())>
                    <button class="primary" disabled=disabled on:click=install>{move || status.with(|s| action_label(s.phase))}</button>
                    <button disabled=disabled on:click=open>"Notes de version"</button>
                </Show>
            </div>
            <div class="hint">"Ember télécharge et installe la mise à jour. Vous choisissez quand redémarrer."</div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_handles_unknown_zero_and_complete_sizes() {
        let mut status = UpdateStatus {
            phase: UpdatePhase::Downloading,
            downloaded_bytes: 5120,
            ..Default::default()
        };
        assert_eq!(description(&status), "Téléchargement… 5 Ko");
        status.total_bytes = Some(0);
        assert_eq!(description(&status), "Téléchargement… 5 Ko");
        status.total_bytes = Some(10240);
        assert_eq!(description(&status), "Téléchargement… 50 %");
        status.downloaded_bytes = 20000;
        assert_eq!(description(&status), "Téléchargement… 100 %");
        assert_eq!(action_label(UpdatePhase::ReadyToRestart), "Redémarrer");
        assert_ne!(action_label(UpdatePhase::Installing), "Redémarrer");
    }

    #[test]
    fn only_a_successful_check_reports_up_to_date() {
        for phase in [
            UpdatePhase::Idle,
            UpdatePhase::Checking,
            UpdatePhase::Failed,
        ] {
            assert_ne!(
                description(&UpdateStatus {
                    phase,
                    ..Default::default()
                }),
                "Ember est à jour"
            );
        }
    }
}

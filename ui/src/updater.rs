//! Le service continue à publier quand les paramètres sont fermés.
use leptos::prelude::*;
use shared::{UpdatePhase, UpdateStatus};

use crate::tauri;

pub fn description(status: &UpdateStatus) -> String {
    match status.phase {
        UpdatePhase::Idle => "Vérification automatique toutes les 10 minutes".into(),
        UpdatePhase::Checking => "Recherche de mises à jour…".into(),
        UpdatePhase::UpToDate => "Ember est à jour".into(),
        UpdatePhase::Available => format!(
            "Ember {} est disponible",
            status.available_version.as_deref().unwrap_or("")
        ),
        UpdatePhase::Downloading => match status.total_bytes.filter(|total| *total > 0) {
            Some(total) => format!(
                "Téléchargement : {:.0} %",
                (status.downloaded_bytes as f64 / total as f64 * 100.0).min(100.0)
            ),
            None => format!(
                "Téléchargement : {:.1} Mo",
                status.downloaded_bytes as f64 / 1_000_000.0
            ),
        },
        UpdatePhase::Installing => {
            "Installation… macOS peut demander une autorisation administrateur".into()
        }
        UpdatePhase::Restarting => "Redémarrage d’Ember…".into(),
        UpdatePhase::Failed => "La mise à jour n’a pas abouti. Vous pouvez réessayer.".into(),
        UpdatePhase::Disabled => "Mises à jour indisponibles pour cette compilation".into(),
    }
}

pub fn accept(status: RwSignal<UpdateStatus>, incoming: UpdateStatus) {
    status.try_update(|current| {
        if incoming.revision >= current.revision {
            *current = incoming;
        }
    });
}

#[component]
pub fn Updates(status: RwSignal<UpdateStatus>) -> impl IntoView {
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<(u64, String)>);
    let disabled = move || {
        pending.get() || status.with(|s| s.phase.busy() || s.phase == UpdatePhase::Disabled)
    };
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
        let Some(version) = status.with(|s| s.available_version.clone()) else {
            return;
        };
        pending.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            if let Err(message) = tauri::install_update(version).await {
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
                <button disabled=disabled on:click=check>"Vérifier les mises à jour"</button>
                <Show when=move || status.with(|s| s.available_version.is_some())>
                    <button class="primary" disabled=disabled on:click=install>"Installer et redémarrer"</button>
                </Show>
            </div>
            <div class="hint">"L’installation conserve l’emplacement de l’application et redémarre Ember."</div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_handles_unknown_zero_and_inexact_lengths() {
        let mut status = UpdateStatus {
            phase: UpdatePhase::Downloading,
            downloaded_bytes: 2_000_000,
            ..Default::default()
        };
        assert_eq!(description(&status), "Téléchargement : 2.0 Mo");
        status.total_bytes = Some(0);
        assert_eq!(description(&status), "Téléchargement : 2.0 Mo");
        status.total_bytes = Some(1_000_000);
        assert_eq!(description(&status), "Téléchargement : 100 %");
    }
    #[test]
    fn only_a_successful_check_reports_up_to_date() {
        for phase in [
            UpdatePhase::Idle,
            UpdatePhase::Checking,
            UpdatePhase::Failed,
            UpdatePhase::Disabled,
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

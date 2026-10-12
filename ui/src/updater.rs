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
        UpdatePhase::Failed => "La vérification n’a pas abouti. Vous pouvez réessayer.".into(),
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
                <button disabled=disabled on:click=check>"Vérifier les mises à jour"</button>
                <Show when=move || status.with(|s| s.available_version.is_some())>
                    <button class="primary" disabled=disabled on:click=open>"Voir la release GitHub"</button>
                </Show>
            </div>
            <div class="hint">"Téléchargez le ZIP depuis GitHub, puis remplacez votre application Ember."</div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

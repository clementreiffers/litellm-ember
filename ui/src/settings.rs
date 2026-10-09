use leptos::prelude::*;
use shared::{KeySource, SettingsInput};

use crate::tauri;

/// Formulaire des paramètres. La clé API n'est jamais relue : le champ est vide et sert uniquement à la remplacer.
#[component]
pub fn Settings(on_close: Callback<()>) -> impl IntoView {
    let base_url = RwSignal::new(String::new());
    let refresh = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());
    let key_source = RwSignal::new(KeySource::Missing);
    let notif_enabled = RwSignal::new(true);
    let notif_info = RwSignal::new(String::new());
    let notif_critical = RwSignal::new(String::new());
    let saving = RwSignal::new(false);
    let message = RwSignal::new(None::<(bool, String)>); // (succès, texte)

    leptos::task::spawn_local(async move {
        let result = tauri::get_settings().await;
        if base_url.is_disposed() {
            return;
        }
        match result {
            Ok(s) => {
                if base_url.is_disposed() {
                    return;
                }
                base_url.set(s.base_url);
                refresh.set(s.refresh_secs.to_string());
                key_source.set(s.key_source);
                notif_enabled.set(s.notifications_enabled);
                notif_info.set(s.notify_info_percent.to_string());
                notif_critical.set(s.notify_critical_percent.to_string());
            }
            Err(error) => message.set(Some((false, error))),
        }
    });

    let save = move |clear_key: bool| {
        if saving.get_untracked() {
            return;
        }
        let Ok(refresh_secs) = refresh.get_untracked().trim().parse::<u32>() else {
            message.set(Some((
                false,
                "La fréquence doit être un nombre entier de secondes".into(),
            )));
            return;
        };
        let (Ok(info), Ok(critical)) = (
            notif_info.get_untracked().trim().parse::<u32>(),
            notif_critical.get_untracked().trim().parse::<u32>(),
        ) else {
            message.set(Some((
                false,
                "Les seuils de notification doivent être des nombres entiers".into(),
            )));
            return;
        };
        let input = SettingsInput {
            notifications_enabled: notif_enabled.get_untracked(),
            notify_info_percent: info,
            notify_critical_percent: critical,
            base_url: base_url.get_untracked(),
            refresh_secs,
            api_key: Some(api_key.get_untracked()),
            clear_key,
        };
        saving.set(true);
        leptos::task::spawn_local(async move {
            let result = tauri::save_settings(&input).await;
            if saving.is_disposed() {
                return;
            }
            saving.set(false);
            match result {
                Ok(view) => {
                    api_key.set(String::new());
                    key_source.set(view.key_source);
                    message.set(Some((true, "Paramètres enregistrés".into())));
                    on_close.run(());
                }
                Err(e) => message.set(Some((false, e))),
            }
        });
    };

    let key_hint = move || match key_source.get() {
        KeySource::Keychain => {
            "Clé enregistrée dans le Trousseau macOS. Laissez vide pour la conserver."
        }
        KeySource::Missing => "Aucune clé enregistrée : saisissez-la ci-dessus.",
    };

    view! {
        <header>
            <div class="caption">"Paramètres"</div>
        </header>
        <section class="card form">
            <label>"Endpoint LiteLLM"
                <input type="url" prop:value=move || base_url.get()
                    on:input=move |e| base_url.set(event_target_value(&e))
                    placeholder="https://…/llm" spellcheck="false" />
            </label>
            <label>"Clé API LiteLLM"
                <input type="password" prop:value=move || api_key.get()
                    on:input=move |e| api_key.set(event_target_value(&e))
                    placeholder="sk-…" autocomplete="off" spellcheck="false" />
            </label>
            <div class="hint">{key_hint}</div>
            <label>"Rafraîchissement (secondes)"
                <input type="number" min="5" max="3600" prop:value=move || refresh.get()
                    on:input=move |e| refresh.set(event_target_value(&e)) />
            </label>
            <h3>"Notifications"</h3>
            <label class="check">
                <input type="checkbox" prop:checked=move || notif_enabled.get()
                    on:change=move |e| notif_enabled.set(event_target_checked(&e)) />
                "Prévenir quand le budget atteint un seuil"
            </label>
            <div class="row">
                <label>"Information (% consommé)"
                    <input type="number" min="1" max="99" prop:value=move || notif_info.get()
                        prop:disabled=move || !notif_enabled.get()
                        on:input=move |e| notif_info.set(event_target_value(&e)) />
                </label>
                <label>"Critique (% consommé)"
                    <input type="number" min="2" max="100" prop:value=move || notif_critical.get()
                        prop:disabled=move || !notif_enabled.get()
                        on:input=move |e| notif_critical.set(event_target_value(&e)) />
                </label>
            </div>
            <div class="hint">
                "Par défaut : 💸 à 50 % (moitié consommée) et ⚠️ à 75 % (il reste 1/4). Une seule notification par seuil et par période de budget."
            </div>
            <button class="test" on:click=move |_| leptos::task::spawn_local(async move {
                    let msg = match tauri::test_notification().await {
                        Ok(()) => (true, "Notification envoyée. Si rien n'apparaît : Réglages Système > Notifications, \
                            autorisez l'app (en mode dev : « Terminal ») et vérifiez le mode Concentration.".to_string()),
                        Err(e) => (false, e),
                    };
                    message.try_set(Some(msg));
                })>
                "Envoyer une notification de test"
            </button>
            {move || message.get().map(|(ok, text)| {
                view! { <div class=if ok { "notice-ok" } else { "error" }>{text}</div> }
            })}
            <div class="actions">
                <button disabled=move || saving.get() class="primary" on:click=move |_| save(false)>"Enregistrer"</button>
                <button disabled=move || saving.get() on:click=move |_| on_close.run(())>"Annuler"</button>
                <button class="danger" on:click=move |_| save(true)
                    disabled=move || saving.get() || key_source.get() != KeySource::Keychain>"Supprimer la clé"</button>
            </div>
        </section>
    }
}

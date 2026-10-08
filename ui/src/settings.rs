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
    let message = RwSignal::new(None::<(bool, String)>); // (succès, texte)

    leptos::task::spawn_local(async move {
        if let Some(s) = tauri::get_settings().await {
            base_url.set(s.base_url);
            refresh.set(s.refresh_secs.to_string());
            key_source.set(s.key_source);
        }
    });

    let save = move |clear_key: bool| {
        let Ok(refresh_secs) = refresh.get_untracked().trim().parse::<u32>() else {
            message.set(Some((false, "La fréquence doit être un nombre entier de secondes".into())));
            return;
        };
        let input = SettingsInput {
            base_url: base_url.get_untracked(),
            refresh_secs,
            api_key: Some(api_key.get_untracked()),
            clear_key,
        };
        leptos::task::spawn_local(async move {
            match tauri::save_settings(&input).await {
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
        KeySource::Keychain => "Clé enregistrée dans le Trousseau macOS. Laissez vide pour la conserver.",
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
            {move || message.get().map(|(ok, text)| {
                view! { <div class=if ok { "notice-ok" } else { "error" }>{text}</div> }
            })}
            <div class="actions">
                <button class="primary" on:click=move |_| save(false)>"Enregistrer"</button>
                <button on:click=move |_| on_close.run(())>"Annuler"</button>
                <button class="danger" on:click=move |_| save(true)
                    disabled=move || key_source.get() != KeySource::Keychain>"Supprimer la clé"</button>
            </div>
        </section>
    }
}

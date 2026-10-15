//! Onglet éphémère : la navigation dépend des tâches, pas des installations.
use crate::tauri;
use leptos::prelude::*;
use shared::{Agent, ContextUsage, Harness, HarnessSettings, HarnessSnapshot};

/// Logos SVG embarqués ; seuls ces assets statiques passent par `inner_html`.
#[component]
fn HarnessLabel(harness: Harness) -> impl IntoView {
    let logo = match harness {
        Harness::Codex => include_str!("../assets/harnesses/codex.svg"),
        Harness::Claude => include_str!("../assets/harnesses/claude.svg"),
        Harness::Opencode => include_str!("../assets/harnesses/opencode.svg"),
    };
    view! {
        <span class="harness-brand">
            <span class=format!("harness-logo harness-logo--{}", harness.key()) aria-hidden="true" inner_html=logo></span>
            <span>{harness.label()}</span>
        </span>
    }
}

pub fn subscribe() -> RwSignal<HarnessSnapshot> {
    let snapshot = RwSignal::new(HarnessSnapshot::default());
    let listener = StoredValue::new_local(None::<tauri::Listener>);
    on_cleanup(move || {
        listener.try_update_value(|v| {
            v.take();
        });
    });
    leptos::task::spawn_local(async move {
        match tauri::on_event("harness-updated", move |next: HarnessSnapshot| {
            snapshot.try_update(|s| s.accept(next));
        })
        .await
        {
            Ok(guard) => {
                let mut guard = Some(guard);
                listener.try_update_value(|v| *v = guard.take());
            }
            Err(error) => {
                snapshot.try_update(|s| {
                    s.statuses.push(shared::HarnessStatus {
                        harness: Harness::Codex,
                        installed: false,
                        available: false,
                        coverage: String::new(),
                        error: Some(error),
                    })
                });
            }
        }
        if let Ok(next) = tauri::get_harness_snapshot().await {
            snapshot.try_update(|s| s.accept(next));
        }
    });
    snapshot
}
/// Une tâche remonte selon son agent le plus urgent : attend l'utilisateur, puis en cours, puis le reste.
fn task_rank(agents: &[Agent], root: &str) -> u8 {
    task_agents(agents, root)
        .iter()
        .map(|a| match a.state {
            shared::AgentState::Waiting => 0,
            shared::AgentState::Running => 1,
            _ => 2,
        })
        .min()
        .unwrap_or(2)
}
/// Ordre stable et tolérance des parents absents/cycliques.
pub fn rows(agents: &[Agent]) -> Vec<(String, usize)> {
    let mut remaining: std::collections::BTreeMap<_, _> =
        agents.iter().map(|a| (a.id.clone(), a)).collect();
    let mut result = vec![];
    while !remaining.is_empty() {
        let root = remaining
            .values()
            .filter(|a| {
                a.parent_id
                    .as_ref()
                    .is_none_or(|p| !remaining.contains_key(p))
            })
            .min_by_key(|a| (task_rank(agents, &a.id), &a.project, a.harness, &a.id))
            .or_else(|| remaining.values().next())
            .map(|a| a.id.clone());
        let Some(root) = root else { break };
        let mut stack = vec![(root, 0)];
        while let Some((id, depth)) = stack.pop() {
            if remaining.remove(&id).is_none() {
                continue;
            }
            let children: Vec<_> = remaining
                .values()
                .filter(|a| a.parent_id.as_ref() == Some(&id))
                .map(|a| (a.id.clone(), depth + 1))
                .collect();
            result.push((id, depth));
            stack.extend(children.into_iter().rev());
        }
    }
    result
}
/// Retire les descendants des tâches repliées ; `rows` est en profondeur d'abord, donc ils suivent leur parent.
fn visible_rows(
    rows: Vec<(String, usize)>,
    collapsed: &std::collections::BTreeSet<String>,
) -> Vec<(String, usize)> {
    let mut hidden_below: Option<usize> = None;
    rows.into_iter()
        .filter(|(id, depth)| {
            if hidden_below.is_some_and(|d| *depth > d) {
                return false;
            }
            hidden_below = collapsed.contains(id).then_some(*depth);
            true
        })
        .collect()
}
fn money(agents: impl Iterator<Item = Agent>) -> String {
    let mut total = 0.;
    let mut known = false;
    let mut partial = false;
    for agent in agents {
        if agent.estimates.is_empty() {
            partial = true;
        }
        for cost in agent.estimates {
            if let Some(value) = cost.usd {
                known = true;
                total += value;
            } else {
                partial = true;
            }
            partial |= cost.partial;
        }
    }
    if known {
        format!(
            "≈ ${total:.4}{}",
            if partial {
                " · estimation partielle"
            } else {
                ""
            }
        )
    } else {
        "Coût indisponible".into()
    }
}
#[component]
pub fn Agents(snapshot: RwSignal<HarnessSnapshot>) -> impl IntoView {
    let clock = RwSignal::new((js_sys::Date::now() / 1000.) as i64);
    if let Ok(timer) = set_interval_with_handle(
        move || {
            clock.try_set((js_sys::Date::now() / 1000.) as i64);
        },
        std::time::Duration::from_secs(1),
    ) {
        on_cleanup(move || timer.clear());
    }
    let collapsed = RwSignal::new(std::collections::BTreeSet::<String>::new());
    let count = move |state: fn(shared::AgentState) -> bool| {
        snapshot.with(|s| s.agents.iter().filter(|a| state(a.state)).count())
    };
    view! {
        <section class="card agents-summary">
            <div class="agents-summary-cost">{move||snapshot.with(|s|money(s.agents.iter().cloned()))}</div>
            <div class="caption">"Équivalent API estimé · distinct de votre facture LiteLLM"</div>
            <div class="agents-counts">
                <span class="agents-count"><i class="harness-dot harness-dot--waiting"></i>{move||format!("{} à vous",count(|s|s==shared::AgentState::Waiting))}</span>
                <span class="agents-count"><i class="harness-dot harness-dot--running"></i>{move||format!("{} en cours",count(|s|s==shared::AgentState::Running))}</span>
                <span class="agents-count"><i class="harness-dot harness-dot--idle"></i>{move||format!("{} au repos",count(|s|!s.active()))}</span>
            </div>
        </section>
        <For each=move||snapshot.with(|s|collapsed.with(|c|visible_rows(rows(&s.agents),c))) key=|(id,_)|id.clone()
            children=move|(id,depth)|view!{<AgentCard id depth snapshot clock collapsed/>}/>
    }
}
#[component]
fn AgentCard(
    id: String,
    depth: usize,
    snapshot: RwSignal<HarnessSnapshot>,
    clock: RwSignal<i64>,
    collapsed: RwSignal<std::collections::BTreeSet<String>>,
) -> impl IntoView {
    let id = StoredValue::new(id);
    let sub_agents = Memo::new(move |_| {
        snapshot.with(|s| {
            task_agents(&s.agents, &id.get_value())
                .len()
                .saturating_sub(1)
        })
    });
    let folded = move || collapsed.with(|c| id.with_value(|id| c.contains(id)));
    let toggle = move |_| {
        collapsed.update(|c| {
            let id = id.get_value();
            if !c.remove(&id) {
                c.insert(id);
            }
        })
    };
    let agent = Memo::new(move |_| {
        snapshot.with(|s| {
            s.agents
                .iter()
                .find(|a| id.with_value(|id| &a.id == id))
                .cloned()
        })
    });
    let get = move |f: fn(&Agent) -> String| agent.with(|a| a.as_ref().map(f).unwrap_or_default());
    view! {
        <section class=move||agent.with(|a|a.as_ref().map(|a|format!("agent-card agent-card--{}{}",dot(a.state),if depth>0{" agent-card--child"}else{""})).unwrap_or_default())
            style:margin-left=format!("{}px",depth.min(4)*14)>
            <div class="agent-top">
                {move||agent.with(|a|a.as_ref().map(|a|view!{<HarnessLabel harness=a.harness/>}))}
                <div class="agent-title">
                    <strong title=move||get(|a|a.title.clone().unwrap_or_default())>{move||get(|a|a.nickname.clone().or(a.title.clone()).unwrap_or_else(||project_name(&a.project)))}</strong>
                    <span class="caption agent-path" title=move||get(|a|a.project.clone())>{move||get(|a|format!("▸ {}",short_path(&a.project)))}</span>
                </div>
                <span class=move||agent.with(|a|a.as_ref().map(|a|format!("agent-pill agent-pill--{}",dot(a.state))).unwrap_or_default())>
                    <i class=move||agent.with(|a|a.as_ref().map(|a|format!("harness-dot harness-dot--{}",dot(a.state))).unwrap_or_default())></i>
                    {move||get(|a|a.state.label().to_owned())}
                </span>
            </div>
            {move||agent.with(|a|a.as_ref().and_then(|a|a.agent_path.clone()).map(|path|view!{<div class="caption">{path}</div>}))}
            <div class="caption">{move||get(|a|format!("Rôle déclaré : {}",a.role.as_deref().unwrap_or("indisponible")))}</div>
            <p class="agent-prompt" title=move||get(|a|a.prompt.clone())>{move||get(|a|if a.prompt_unavailable && a.prompt.is_empty(){"Mission chiffrée · indisponible dans les journaux".into()}else{excerpt(&a.prompt)})}</p>
            <div class="agent-meta">
                <span>{move||agent.with(|a|a.as_ref().map(|a|{
                    let model=a.model.clone().unwrap_or_else(||"Modèle indisponible".into());
                    if a.state.active(){format!("{model} · {} depuis {}",duration(clock.get()-a.started_at),format_start(a.started_at))}else{format!("{model} · début {}",format_start(a.started_at))}
                }).unwrap_or_default())}</span>
                <span class="agent-cost">{move||agent.with(|a|compact_money(a.iter().cloned()))}</span>
            </div>
            {move||agent.with(|a|a.as_ref().and_then(|a|a.tool.clone()).map(|t|view!{<div class="agent-chips"><span class="agent-chip agent-chip--tool">{format!("⚙ {t}")}</span></div>}))}
            {move||agent.with(|a|a.as_ref().and_then(|a|a.context.clone()).map(|c|view!{<ContextGauge context=c/>}))}
            <div class="agent-chips">
                <span class="agent-label">"Skills"</span>
                {move||agent.with(|a|a.as_ref().map(|a|if a.skills.is_empty(){
                    view!{<span class="caption">"aucun observé"</span>}.into_any()
                }else{
                    a.skills.iter().map(|s|view!{<span class=if s.explicit{"agent-chip agent-chip--used"}else{"agent-chip"} title=if s.explicit{"Skill invoqué"}else{"Fichier du skill consulté"}>{format!("✦ {}",s.name)}</span>}).collect_view().into_any()
                }))}
            </div>
            <details class="agent-details">
                <summary>"Détails"</summary>
                {(depth==0).then(||view!{<div class="agent-row"><span>"Tâche avec sous-agents"</span><span>{move||snapshot.with(|s|compact_money(task_agents(&s.agents,&id.get_value()).into_iter()))}</span></div>})}
                <div class="agent-row"><span>"Identifiant"</span><span>{move||get(|a|a.id.clone())}</span></div>
                <div class="caption">"Mission observée · contexte mesuré en tokens ci-dessus"</div>
                {move||agent.with(|a|a.as_ref().is_some_and(|a|a.prompt_unavailable)).then(||view!{<p class="caption">"Le contenu de la mission est chiffré par Codex dans ses journaux locaux."</p>})}
                <pre class="agent-full-prompt">{move||get(|a|a.prompt.clone())}</pre>
                {move||agent.with(|a|a.as_ref().map(|a|a.estimates.iter().map(|e|{
                    let rates=e.rates.map(|r|r.map(|n|format!("${n:.2}")).unwrap_or_else(||"—".into())).join(" · ");
                    view!{<div class="agent-model">
                        <div class="agent-row"><strong>{e.model.clone()}</strong><span>{e.usd.map(|u|format!("${u:.4}")).unwrap_or_else(||"—".into())}</span></div>
                        <div class="agent-tokens">
                            <span><b>{compact(e.tokens.input)}</b>" entrée"</span>
                            <span><b>{compact(e.tokens.output)}</b>" sortie"</span>
                            <span><b>{compact(e.tokens.cache_read)}</b>" cache lu"</span>
                            <span><b>{compact(e.tokens.cache_write)}</b>" cache écrit"</span>
                        </div>
                        <div class="caption">{format!("$/M : {rates} · {}",e.source.clone().unwrap_or_else(||"tarif indisponible".into()))}</div>
                    </div>}
                }).collect_view()))}
            </details>
                {move||(sub_agents.get()>0).then(||view!{
                    <button class=move||if folded(){"agent-fold agent-fold--folded"}else{"agent-fold"} type="button" on:click=toggle
                        aria-expanded=move||(!folded()).to_string()
                        title=move||if folded(){"Afficher les sous-agents"}else{"Masquer les sous-agents"}>
                        <svg class="agent-fold-chevron" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                            <path d="M5 6.5 8 9.5 11 6.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
                        </svg>
                        {move||format!("{} sous-agent{}",sub_agents.get(),if sub_agents.get()>1{"s"}else{""})}
                    </button>
                })}
        </section>
    }
}
/// Jauge + courbe du remplissage du contexte ; la couleur suit la saturation.
#[component]
fn ContextGauge(context: ContextUsage) -> impl IntoView {
    let ratio = context.ratio();
    let level = match ratio {
        Some(r) if r >= 0.85 => "high",
        Some(r) if r >= 0.6 => "mid",
        _ => "low",
    };
    let label = match (ratio, context.window) {
        (Some(r), Some(w)) => format!(
            "{:.0} % · {} / {}",
            r * 100.,
            compact(context.used),
            compact(w)
        ),
        _ => format!("{} tokens", compact(context.used)),
    };
    let points = sparkline(&context.history, context.window);
    view! {
        <div class=format!("agent-context agent-context--{level}") title="Taille du contexte de la dernière requête">
            <div class="agent-meta"><span class="agent-label">"Contexte"</span><span>{label}</span></div>
            <div class="track"><div class="fill" style:width=format!("{:.1}%",ratio.unwrap_or(0.)*100.)></div></div>
            {(context.history.len()>1).then(||view!{
                <svg class="agent-spark" viewBox="0 0 100 24" preserveAspectRatio="none" aria-hidden="true">
                    <polyline points=points fill="none" stroke="currentColor" stroke-width="1.5" vector-effect="non-scaling-stroke"/>
                </svg>
            })}
        </div>
    }
}
/// Points SVG (viewBox 100×24) ; l'échelle est la fenêtre si elle est connue, sinon le maximum observé.
fn sparkline(history: &[u64], window: Option<u64>) -> String {
    let top = window
        .unwrap_or_else(|| history.iter().copied().max().unwrap_or(1))
        .max(1) as f64;
    let step = 100. / (history.len().max(2) - 1) as f64;
    history
        .iter()
        .enumerate()
        .map(|(i, v)| {
            format!(
                "{:.1},{:.1}",
                i as f64 * step,
                23. - (*v as f64 / top).min(1.) * 22.
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}
/// Chemin de démarrage lisible : le dossier personnel devient `~`.
fn short_path(path: &str) -> String {
    let mut parts = path.splitn(4, '/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(""), Some("Users" | "home"), Some(_), rest) => format!("~/{}", rest.unwrap_or("")),
        _ if path.is_empty() => "dossier inconnu".into(),
        _ => path.to_owned(),
    }
}
fn project_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("Projet inconnu")
        .to_owned()
}
fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}
fn compact_money(agents: impl Iterator<Item = Agent>) -> String {
    money(agents).replace(" · estimation partielle", " +")
}
/// Pastille : orange = besoin de vous, vert = en cours, gris = terminé ou inactif.
fn dot(state: shared::AgentState) -> &'static str {
    match state {
        shared::AgentState::Waiting => "waiting",
        shared::AgentState::Running => "running",
        _ => "idle",
    }
}
fn task_agents(agents: &[Agent], root: &str) -> Vec<Agent> {
    let mut selected = std::collections::BTreeSet::from([root.to_owned()]);
    loop {
        let before = selected.len();
        for a in agents {
            if a.parent_id.as_ref().is_some_and(|p| selected.contains(p)) {
                selected.insert(a.id.clone());
            }
        }
        if selected.len() == before {
            break;
        }
    }
    agents
        .iter()
        .filter(|a| selected.contains(&a.id))
        .cloned()
        .collect()
}
fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        format!("{} min {} s", seconds / 60, seconds % 60)
    } else {
        format!("{} h {} min", seconds / 3600, (seconds % 3600) / 60)
    }
}
fn excerpt(prompt: &str) -> String {
    if prompt.is_empty() {
        "Prompt indisponible".into()
    } else {
        let mut text: String = prompt.chars().take(140).collect();
        if prompt.chars().count() > 140 {
            text.push('…');
        }
        text
    }
}
fn format_start(timestamp: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp as f64 * 1000.));
    format!("{:02}:{:02}", date.get_hours(), date.get_minutes())
}
#[component]
pub fn HarnessPreferences(snapshot: RwSignal<HarnessSnapshot>) -> impl IntoView {
    let settings = RwSignal::new(HarnessSettings::default());
    let loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let message = RwSignal::new(String::new());
    let secret = RwSignal::new(String::new());
    leptos::task::spawn_local(async move {
        match tauri::get_harness_settings().await {
            Ok(cfg) => {
                settings.try_set(cfg);
                loaded.try_set(true);
            }
            Err(error) => {
                message.try_set(error);
            }
        }
    });
    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        saving.set(true);
        let input = settings.get_untracked();
        leptos::task::spawn_local(async move {
            match tauri::save_harness_settings(&input).await {
                Ok(cfg) => {
                    settings.try_set(cfg);
                    message.try_set("Préférences enregistrées.".into());
                }
                Err(error) => {
                    message.try_set(error);
                }
            }
            saving.try_set(false);
        });
    };
    let save_secret = move |clear: bool| {
        let value = if clear {
            None
        } else {
            Some(secret.get_untracked())
        };
        saving.set(true);
        leptos::task::spawn_local(async move {
            let result = tauri::save_opencode_password(value).await;
            secret.try_set(String::new());
            message.try_set(match result {
                Ok(()) => "Trousseau mis à jour".into(),
                Err(e) => e,
            });
            saving.try_set(false);
        });
    };
    view! {
        <section class="card form">
            <h3>"Harnais"</h3>
            <p class="hint">"Suivi local des tâches en cours. Les prompts restent en mémoire."</p>
            {Harness::ALL.into_iter().map(move|h|view!{
                <fieldset class="harness-config" disabled=move||!loaded.get()||saving.get()>
                    <legend><HarnessLabel harness=h/></legend>
                    <div class="hint">{move||snapshot.with(|s|s.statuses.iter().find(|s|s.harness==h).map(|s|format!("{} · {}{}",if s.installed{"Détecté"}else{"Non détecté"},s.coverage,s.error.as_ref().map(|e|format!(" — {e}")).unwrap_or_default())).unwrap_or_else(||"Détection…".into()))}</div>
                    <label class="check"><input type="checkbox" prop:checked=move||settings.with(|s|!s.get(h).disabled) on:change=move|e|settings.update(|s|{let c=s.get_mut(h);c.disabled = !event_target_checked(&e);if c.disabled{c.hooks=false;}})/>"Activer le suivi"</label>
                    <label>"Répertoire de données personnalisé"<input type="text" placeholder="Automatique" prop:value=move||settings.with(|s|s.get(h).root.clone()) on:input=move|e|settings.update(|s|s.get_mut(h).root=event_target_value(&e))/></label>
                </fieldset>
            }).collect_view()}
            <label>"Serveur OpenCode local"<input type="url" placeholder="Détection automatique ou http://127.0.0.1:4096" prop:value=move||settings.with(|s|s.opencode.endpoint.clone()) on:input=move|e|settings.update(|s|s.opencode.endpoint=event_target_value(&e))/></label>
            <button disabled=move||!loaded.get()||saving.get() on:click=save>"Enregistrer le suivi"</button>
            <label>"Mot de passe OpenCode (utilisateur opencode)"<input type="password" autocomplete="off" prop:value=move||secret.get() on:input=move|e|secret.set(event_target_value(&e))/></label>
            <div class="hint">"Facultatif. Stocké uniquement dans le Trousseau macOS."</div>
            <div class="harness-heading"><button disabled=move||saving.get()||secret.with(String::is_empty) on:click=move |_|save_secret(false)>"Enregistrer le secret"</button><button disabled=move||saving.get() on:click=move |_|save_secret(true)>"Supprimer le secret"</button></div>
            <p role="status" class="hint">{move||message.get()}</p>
        </section>
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn running_tasks_are_listed_before_idle_ones() {
        let agent = |id: &str, state| Agent {
            id: id.into(),
            parent_id: None,
            harness: Harness::Codex,
            project: "/a".into(),
            title: None,
            prompt: String::new(),
            agent_path: None,
            nickname: None,
            role: None,
            prompt_unavailable: false,
            model: None,
            state,
            started_at: 0,
            tool: None,
            skills: vec![],
            estimates: vec![],
            context: None,
        };
        let agents = [
            agent("a", shared::AgentState::Completed),
            agent("b", shared::AgentState::Running),
            agent("c", shared::AgentState::Waiting),
        ];
        let order: Vec<_> = rows(&agents).into_iter().map(|(id, _)| id).collect();
        assert_eq!(order, ["c", "b", "a"]);
    }
    #[test]
    fn sparkline_scales_to_the_window() {
        assert_eq!(
            sparkline(&[0, 50, 100], Some(100)),
            "0.0,23.0 50.0,12.0 100.0,1.0"
        );
        assert_eq!(sparkline(&[10, 20], None), "0.0,12.0 100.0,1.0");
    }
    #[test]
    fn paths_are_shortened_with_tilde() {
        assert_eq!(short_path("/Users/me/work/app"), "~/work/app");
        assert_eq!(short_path("/Users/me"), "~/");
        assert_eq!(short_path("/opt/app"), "/opt/app");
        assert_eq!(short_path(""), "dossier inconnu");
    }
    #[test]
    fn helpers_format_compactly() {
        assert_eq!(project_name("/a/b/ember/"), "ember");
        assert_eq!(project_name(""), "Projet inconnu");
        assert_eq!(compact(1500), "1.5k");
        assert_eq!(compact(2_000_000), "2.0M");
    }
    #[test]
    fn dots_follow_state() {
        use shared::AgentState::*;
        assert_eq!(dot(Waiting), "waiting");
        assert_eq!(dot(Running), "running");
        assert_eq!(dot(Completed), "idle");
    }
    #[test]
    fn excerpts_preserve_unicode() {
        assert_eq!(excerpt("é🙂"), "é🙂");
        assert!(excerpt(&"é".repeat(141)).ends_with('…'));
    }
    #[test]
    fn collapsed_tasks_hide_all_their_descendants() {
        let rows = vec![
            ("a".to_owned(), 0),
            ("b".to_owned(), 1),
            ("c".to_owned(), 2),
            ("d".to_owned(), 1),
            ("e".to_owned(), 0),
        ];
        let ids = |r: Vec<(String, usize)>| r.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        let none = std::collections::BTreeSet::new();
        assert_eq!(
            ids(visible_rows(rows.clone(), &none)),
            ["a", "b", "c", "d", "e"]
        );
        let top = ["a".to_owned()].into();
        assert_eq!(ids(visible_rows(rows.clone(), &top)), ["a", "e"]);
        let nested = ["b".to_owned()].into();
        assert_eq!(ids(visible_rows(rows, &nested)), ["a", "b", "d", "e"]);
    }
    #[test]
    fn obsolete_snapshot_does_not_reopen_tab() {
        let mut s = HarnessSnapshot {
            revision: 3,
            ..Default::default()
        };
        s.accept(HarnessSnapshot {
            revision: 2,
            ..Default::default()
        });
        assert_eq!(s.revision, 3);
    }
}

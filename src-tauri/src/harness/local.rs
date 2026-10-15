//! Surveillance incrémentale bornée des transcriptions et processus de l'utilisateur.
use super::engine::{now, Engine};
use shared::{Harness, HarnessSettings, HarnessStatus};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}
pub fn root(h: Harness, cfg: &HarnessSettings) -> PathBuf {
    let custom = &cfg.get(h).root;
    if !custom.is_empty() {
        return PathBuf::from(custom);
    }
    match h {
        Harness::Codex => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".codex")),
        Harness::Claude => std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".claude")),
        Harness::Opencode => std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("opencode"),
    }
}
#[derive(Default)]
pub struct Processes {
    pub available: bool,
    pub live: BTreeSet<u32>,
    pub parents: BTreeMap<u32, u32>,
    pub harnesses: BTreeMap<u32, Harness>,
    pub files: BTreeMap<PathBuf, u32>,
    pub servers: BTreeSet<String>,
    pub server_owners: BTreeMap<String, u32>,
    pub directories: BTreeMap<u32, PathBuf>,
    pub opencode: Option<super::opencode::LocalService>,
}
async fn command(program: &str, args: &[String]) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(3), cmd.output())
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
impl Processes {
    pub async fn read() -> Self {
        let mut result = Self::default();
        if let Some(output) = command("/bin/ps", &["-axo".into(), "pid=,ppid=,comm=".into()]).await
        {
            for line in output.lines() {
                let mut fields = line.split_whitespace();
                let (Some(pid), Some(ppid)) = (
                    fields.next().and_then(|s| s.parse::<u32>().ok()),
                    fields.next().and_then(|s| s.parse::<u32>().ok()),
                ) else {
                    continue;
                };
                result.live.insert(pid);
                result.parents.insert(pid, ppid);
                let name = fields.collect::<Vec<_>>().join(" ").to_lowercase();
                let base = Path::new(&name)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                let harness = if base == "codex" || name.contains("codex.app/") {
                    Some(Harness::Codex)
                } else if base == "claude" {
                    Some(Harness::Claude)
                } else if base == "opencode" {
                    Some(Harness::Opencode)
                } else {
                    None
                };
                if let Some(h) = harness {
                    result.harnesses.insert(pid, h);
                }
            }
        }
        result.available = !result.live.is_empty();
        if !result.harnesses.is_empty() {
            let pids = result
                .harnesses
                .keys()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            if let Some(output) = command(
                "/usr/sbin/lsof",
                &["-nP".into(), "-a".into(), "-p".into(), pids, "-FpnT".into()],
            )
            .await
            {
                let mut pid = 0;
                let mut address = None;
                let mut cwd = false;
                for line in output.lines() {
                    if let Some(value) = line.strip_prefix('p') {
                        pid = value.parse().unwrap_or(0);
                    }
                    if let Some(value) = line.strip_prefix('f') {
                        cwd = value == "cwd";
                        address = None;
                    }
                    if let Some(value) = line.strip_prefix('n') {
                        if cwd {
                            result.directories.insert(pid, PathBuf::from(value));
                        }
                        address = None;
                        if value.starts_with('/') {
                            result.files.insert(PathBuf::from(value), pid);
                        }
                        if result.harnesses.get(&pid) == Some(&Harness::Opencode) {
                            if let Some(port) = value
                                .strip_prefix("127.0.0.1:")
                                .or_else(|| value.strip_prefix("[::1]:"))
                            {
                                if let Ok(port) = port.parse::<u16>() {
                                    address = Some(format!("http://127.0.0.1:{port}"));
                                }
                            }
                        }
                    }
                    if line == "TST=LISTEN" {
                        if let Some(url) = address.take() {
                            result.server_owners.insert(url.clone(), pid);
                            result.servers.insert(url);
                        }
                    }
                }
            }
        }
        result.opencode = super::opencode::discover(&result);
        result
    }
    pub fn owner(&self, pid: u32, h: Harness) -> Option<u32> {
        let mut current = pid;
        for _ in 0..12 {
            if self.harnesses.get(&current) == Some(&h) {
                return Some(current);
            }
            current = *self.parents.get(&current)?;
        }
        None
    }
    fn file_owner(&self, path: &Path) -> Option<u32> {
        if let Some(pid) = self.files.get(path) {
            return Some(*pid);
        }
        let stem = path.file_stem()?.to_str()?;
        let session = if stem.starts_with("rollout-") && stem.len() >= 36 {
            &stem[stem.len() - 36..]
        } else {
            stem
        };
        self.files
            .iter()
            .find(|(p, _)| {
                p.extension().is_some_and(|s| s == "lock")
                    && p.file_stem().and_then(|s| s.to_str()) == Some(session)
            })
            .map(|(_, pid)| *pid)
    }
}
#[derive(Default)]
struct Cursor {
    offset: u64,
    pending: Vec<u8>,
    session: String,
    modified: Option<SystemTime>,
    inode: u64,
    ordinal: u64,
    history_start: u64,
    metadata: Option<serde_json::Value>,
}
#[derive(Default)]
pub struct Reader {
    cursors: BTreeMap<PathBuf, Cursor>,
    pub errors: BTreeMap<Harness, String>,
}
impl Reader {
    pub fn scan(
        &mut self,
        cfg: &HarnessSettings,
        processes: &Processes,
        engine: &mut Engine,
    ) -> Vec<HarnessStatus> {
        let mut statuses = vec![];
        for h in Harness::ALL {
            let path = root(h, cfg);
            let installed = path.is_dir()
                || processes.harnesses.values().any(|v| *v == h)
                || executable(h.key());
            if !cfg.get(h).disabled && h != Harness::Opencode {
                let source = path.join(if h == Harness::Codex {
                    "sessions"
                } else {
                    "projects"
                });
                let mut files = Vec::new();
                walk(&source, 0, &mut files);
                for open in processes.files.keys().filter(|p| {
                    p.starts_with(&source) && p.extension().is_some_and(|e| e == "jsonl")
                }) {
                    if !files.contains(open) {
                        files.push(open.clone());
                    }
                }
                self.errors.remove(&h);
                for file in files {
                    if let Err(error) = self.read(h, &file, processes, engine) {
                        self.errors.insert(h, error);
                    }
                }
                if h == Harness::Claude {
                    attach_by_cwd(h, processes, engine);
                }
                if h == Harness::Codex {
                    apply_codex_names(&path.join("session_index.jsonl"), engine);
                }
            }
            statuses.push(HarnessStatus {
                harness: h,
                installed,
                available: installed && !cfg.get(h).disabled,
                coverage: if cfg.get(h).disabled {
                    "Désactivé"
                } else if h == Harness::Opencode {
                    "API locale · connexion requise"
                } else if cfg.get(h).hooks {
                    "Journaux + hooks · vérifier la confiance dans le harnais"
                } else {
                    "Journaux · attentes et sous-agents selon les événements disponibles"
                }
                .into(),
                error: self.errors.get(&h).cloned(),
            });
        }
        self.cursors.retain(|p, _| p.exists());
        statuses
    }
    fn read(
        &mut self,
        h: Harness,
        path: &Path,
        processes: &Processes,
        engine: &mut Engine,
    ) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(path).map_err(|_| "Transcription inaccessible")?;
        let owner = processes.file_owner(path);
        let cursor = self.cursors.entry(path.to_owned()).or_default();
        let changed = cursor.modified != meta.modified().ok();
        if cursor.inode != meta.ino() || cursor.offset > meta.len() {
            *cursor = Cursor::default();
            cursor.inode = meta.ino();
        }
        // L'entrée peut avoir été purgée pendant le rattrapage d'un gros fork.
        if h == Harness::Codex
            && !engine
                .entries
                .contains_key(&super::engine::key(h, &cursor.session))
        {
            if let Some(metadata) = &cursor.metadata {
                engine.codex(&mut cursor.session, metadata, owner.is_some(), owner);
            }
        }
        if !changed && cursor.offset == meta.len() {
            if let Some(e) = engine
                .entries
                .get_mut(&super::engine::key(h, &cursor.session))
            {
                if owner.is_some() {
                    e.owner = owner;
                }
            }
            return Ok(());
        }
        let initial = cursor.modified.is_none();
        let mut file = File::open(path).map_err(|_| "Transcription inaccessible")?;
        const LIMIT: u64 = 2 * 1024 * 1024;
        // Codex : lecture depuis le début, par blocs bornés. Les forks ont un préfixe
        // hérité dont la frontière est un ordinal, et les métadonnées sont au début.
        let skip = initial && meta.len() > LIMIT && h != Harness::Codex;
        if skip {
            cursor.offset = meta.len() - LIMIT;
        }
        file.seek(SeekFrom::Start(cursor.offset))
            .map_err(|_| "Lecture impossible")?;
        let mut bytes = Vec::new();
        file.take(LIMIT)
            .read_to_end(&mut bytes)
            .map_err(|_| "Lecture impossible")?;
        cursor.offset += bytes.len() as u64;
        if skip {
            if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                bytes.drain(..=end);
            }
        }
        cursor.pending.extend(bytes);
        let end = cursor
            .pending
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let child = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("agent-"));
        let recent = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d < Duration::from_secs(10));
        let live = owner.is_some() || (!initial && recent);
        for line in cursor.pending[..end].split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                if h == Harness::Codex {
                    if cursor.ordinal == 0 {
                        cursor.history_start = value["payload"]["subagent_history_start_ordinal"]
                            .as_u64()
                            .unwrap_or(0);
                        // Ne conserve pas les instructions système du session_meta.
                        let mut metadata = serde_json::json!({"type":"session_meta", "payload":{}});
                        for field in [
                            "id",
                            "cwd",
                            "parent_thread_id",
                            "source",
                            "agent_path",
                            "agent_role",
                            "agent_nickname",
                        ] {
                            metadata["payload"][field] = value["payload"][field].clone();
                        }
                        cursor.metadata = Some(metadata);
                    }
                    let ordinal = value["ordinal"].as_u64().unwrap_or(cursor.ordinal);
                    cursor.ordinal += 1;
                    if ordinal > 0 && ordinal < cursor.history_start {
                        continue;
                    }
                    engine.codex(&mut cursor.session, &value, live, owner);
                } else {
                    cursor.session = child
                        .map(str::to_owned)
                        .unwrap_or_else(|| super::engine::string(&value, "sessionId"));
                    engine.claude(&value, live, owner, child);
                }
            }
        }
        cursor.pending.drain(..end);
        if skip || h == Harness::Codex {
            if let Some(e) = engine
                .entries
                .get_mut(&super::engine::key(h, &cursor.session))
            {
                e.partial_usage = skip || cursor.offset < meta.len() || !cursor.pending.is_empty();
            }
        }
        if cursor.pending.len() > LIMIT as usize {
            cursor.pending.clear();
            return Err("Événement trop volumineux ; suivi partiel".into());
        }
        cursor.modified = meta.modified().ok();
        Ok(())
    }
}
/// Claude Code ne garde pas sa transcription ouverte : on rattache la session au processus
/// du harnais dont le répertoire courant est le projet de la session.
fn attach_by_cwd(h: Harness, processes: &Processes, engine: &mut Engine) {
    for e in engine.entries.values_mut() {
        if e.agent.harness != h || e.owner.is_some() || e.agent.project.is_empty() {
            continue;
        }
        e.owner = processes
            .files
            .get(Path::new(&e.agent.project))
            .copied()
            .filter(|pid| processes.harnesses.get(pid) == Some(&h));
    }
}
/// Noms des fils Codex (`session_index.jsonl`, une ligne par renommage : la dernière l'emporte).
fn apply_codex_names(index: &Path, engine: &mut Engine) {
    let Ok(file) = File::open(index) else { return };
    let mut text = String::new();
    // Fichier court ; la borne protège d'un index anormalement volumineux.
    if file
        .take(4 * 1024 * 1024)
        .read_to_string(&mut text)
        .is_err()
    {
        return;
    }
    let mut names = BTreeMap::new();
    for line in text.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let (Some(id), Some(name)) = (v["id"].as_str(), v["thread_name"].as_str()) {
                names.insert(id.to_owned(), name.to_owned());
            }
        }
    }
    for (id, name) in names {
        if let Some(e) = engine
            .entries
            .get_mut(&super::engine::key(Harness::Codex, &id))
        {
            e.agent.title = super::engine::non_empty(name);
        }
    }
}
fn executable(name: &str) -> bool {
    [
        home().join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]
    .iter()
    .any(|p| p.join(name).is_file())
}
fn walk(path: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > 6 || files.len() >= 512 {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= 512 {
            break;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if entry.file_type().is_ok_and(|t| t.is_symlink()) {
            continue;
        }
        if meta.is_dir() {
            walk(&entry.path(), depth + 1, files);
        } else if entry.path().extension().is_some_and(|e| e == "jsonl")
            && meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < Duration::from_secs(86400))
        {
            files.push(entry.path());
        }
    }
}

pub fn has_unknown(engine: &Engine, h: Harness) -> bool {
    engine.entries.values().any(|e| {
        e.agent.harness == h
            && e.agent.state == shared::AgentState::Unknown
            && now() - e.touched < 60
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("ember-harness-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn row(kind: &str, payload: serde_json::Value) -> String {
        format!(
            "{}\n",
            json!({"timestamp":chrono::Utc::now().to_rfc3339(),"type":kind,"payload":payload})
        )
    }
    #[test]
    fn codex_thread_names_use_the_latest_rename() {
        let dir = Fixture::new();
        let index = dir.0.join("session_index.jsonl");
        fs::write(
            &index,
            "{\"id\":\"a\",\"thread_name\":\"Old\"}\n{\"id\":\"a\",\"thread_name\":\"New\"}\nnot json\n",
        )
        .unwrap();
        let mut engine = Engine::default();
        engine.entry(Harness::Codex, "a");
        apply_codex_names(&index, &mut engine);
        assert_eq!(
            engine.entries["codex:a"].agent.title.as_deref(),
            Some("New")
        );
    }
    #[test]
    fn claude_session_is_attached_to_the_process_running_in_its_project() {
        let mut engine = Engine::default();
        let entry = engine.entry(Harness::Claude, "s");
        entry.agent.project = "/work/app".into();
        let mut processes = Processes::default();
        processes.harnesses.insert(7, Harness::Claude);
        processes.files.insert(PathBuf::from("/work/app"), 7);
        attach_by_cwd(Harness::Claude, &processes, &mut engine);
        assert_eq!(engine.entries.values().next().unwrap().owner, Some(7));
    }
    #[test]
    fn partial_lines_are_completed_once_and_truncation_resets_cursor() {
        let dir = Fixture::new();
        let path = dir.0.join("rollout-test.jsonl");
        let start = row("session_meta", json!({"id":"s","cwd":"/project"}))
            + &row("event_msg", json!({"type":"task_started","turn_id":"a"}));
        let usage = row(
            "token_usage_record",
            json!({"response_id":"r","usage":{"input_tokens":12,"output_tokens":4}}),
        );
        fs::write(&path, format!("{}{}", start, &usage[..usage.len() / 2])).unwrap();
        let mut processes = Processes::default();
        processes.files.insert(path.clone(), 42);
        processes.live.insert(42);
        let mut reader = Reader::default();
        let mut engine = Engine::default();
        reader
            .read(Harness::Codex, &path, &processes, &mut engine)
            .unwrap();
        assert!(engine.entries["codex:s"].usage.is_empty());
        assert!(engine.entries["codex:s"].partial_usage);
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&usage.as_bytes()[usage.len() / 2..])
            .unwrap();
        reader
            .read(Harness::Codex, &path, &processes, &mut engine)
            .unwrap();
        reader
            .read(Harness::Codex, &path, &processes, &mut engine)
            .unwrap();
        assert_eq!(engine.entries["codex:s"].usage.len(), 1);
        assert!(!engine.entries["codex:s"].partial_usage);
        fs::write(
            &path,
            row("session_meta", json!({"id":"s"}))
                + &row("event_msg", json!({"type":"task_complete"})),
        )
        .unwrap();
        reader
            .read(Harness::Codex, &path, &processes, &mut engine)
            .unwrap();
        assert!(engine
            .snapshot(&super::super::pricing::Prices::default())
            .is_empty());
    }
    #[test]
    fn codex_fork_keeps_identity_and_excludes_inherited_usage_skills_and_prompts() {
        let dir = Fixture::new();
        let path = dir.0.join("fork.jsonl");
        let records = [
            row(
                "session_meta",
                json!({"id":"child","cwd":"/project","parent_thread_id":"parent","agent_path":"/root/dev","agent_nickname":"Ada","subagent_history_start_ordinal":6}),
            ),
            row("session_meta", json!({"id":"parent"})),
            row(
                "event_msg",
                json!({"type":"task_started","turn_id":"parent-turn"}),
            ),
            row(
                "response_item",
                json!({"type":"message","role":"user","content":"Parent prompt"}),
            ),
            row(
                "response_item",
                json!({"type":"function_call","name":"exec","arguments":"cat /skills/parent/SKILL.md"}),
            ),
            row(
                "token_usage_record",
                json!({"thread_id":"parent","response_id":"parent-response","usage":{"input_tokens":900}}),
            ),
            row(
                "event_msg",
                json!({"type":"task_started","turn_id":"child-turn"}),
            ),
            row("turn_context", json!({"model":"gpt-5"})),
            row(
                "response_item",
                json!({"type":"agent_message","recipient":"/root/dev","content":[{"type":"input_text","text":"Message Type: NEW_TASK\nPayload:\nFix child"}]}),
            ),
            row(
                "token_usage_record",
                json!({"thread_id":"child","response_id":"child-response","usage":{"input_tokens":100}}),
            ),
            row(
                "event_msg",
                json!({"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"model_context_window":1000}}),
            ),
        ];
        fs::write(&path, records.concat()).unwrap();
        let mut processes = Processes::default();
        processes.files.insert(path.clone(), 42);
        let mut reader = Reader::default();
        let mut engine = Engine::default();
        for _ in 0..2 {
            reader
                .read(Harness::Codex, &path, &processes, &mut engine)
                .unwrap();
        }
        assert!(!engine.entries.contains_key("codex:parent"));
        let entry = &engine.entries["codex:child"];
        assert_eq!(entry.agent.parent_id.as_deref(), Some("codex:parent"));
        assert_eq!(entry.agent.nickname.as_deref(), Some("Ada"));
        assert_eq!(entry.agent.prompt, "Fix child");
        assert!(entry.agent.skills.is_empty());
        assert_eq!(entry.usage.len(), 1);
        assert_eq!(entry.usage["child-response"].1.input, 100);
        assert_eq!(entry.agent.context.as_ref().unwrap().used, 100);
    }
    #[test]
    fn large_codex_fork_restores_metadata_after_reconciliation_between_blocks() {
        let dir = Fixture::new();
        let path = dir.0.join("large-fork.jsonl");
        let mut data = row(
            "session_meta",
            json!({"id":"child","parent_thread_id":"parent","agent_path":"/root/dev","subagent_history_start_ordinal":5}),
        );
        // Plusieurs lignes héritées : aucune allocation de ligne au-delà de la borne.
        for _ in 0..4 {
            data += &row(
                "response_item",
                json!({"type":"message","role":"user","content":"x".repeat(600_000)}),
            );
        }
        data += &row(
            "event_msg",
            json!({"type":"task_started","turn_id":"child-turn"}),
        );
        data += &row(
            "response_item",
            json!({"type":"agent_message","recipient":"/root/dev","content":[{"type":"input_text","text":"Message Type: NEW_TASK\nPayload:\nMission"}]}),
        );
        fs::write(&path, data).unwrap();
        let mut reader = Reader::default();
        let mut engine = Engine::default();
        let processes = Processes::default();
        reader
            .read(Harness::Codex, &path, &processes, &mut engine)
            .unwrap();
        engine.reconcile(&BTreeSet::new());
        assert!(!engine.entries.contains_key("codex:child"));
        reader
            .read(Harness::Codex, &path, &processes, &mut engine)
            .unwrap();
        let entry = &engine.entries["codex:child"];
        assert_eq!(entry.agent.parent_id.as_deref(), Some("codex:parent"));
        assert_eq!(entry.agent.agent_path.as_deref(), Some("/root/dev"));
        assert_eq!(entry.agent.prompt, "Mission");
        assert!(!entry.partial_usage);
    }
    #[test]
    fn a_lock_identifies_owner_without_relying_on_recent_modification() {
        let mut p = Processes::default();
        p.files.insert(
            PathBuf::from(
                "/home/.codex/thread-writer-locks/00000000-0000-0000-0000-000000000001.lock",
            ),
            42,
        );
        assert_eq!(p.file_owner(Path::new("/home/.codex/sessions/rollout-2026-10-13-00000000-0000-0000-0000-000000000001.jsonl")),Some(42));
    }
    #[tokio::test]
    #[ignore = "Diagnostic explicite : lit les sessions locales sans afficher leur contenu"]
    async fn live_local_discovery() {
        let processes = Processes::read().await;
        assert!(!processes.live.is_empty(), "Processus inaccessibles");
        println!(
            "Open files: {}, writer locks: {}",
            processes.files.len(),
            processes
                .files
                .keys()
                .filter(|p| p.to_string_lossy().contains("thread-writer-locks"))
                .count()
        );
        let mut engine = Engine::default();
        let mut reader = Reader::default();
        let statuses = reader.scan(&HarnessSettings::default(), &processes, &mut engine);
        for h in Harness::ALL {
            println!("{} before reconcile: owners={}, running={}, unknown={}",h.label(),engine.entries.values().filter(|e|e.agent.harness==h&&e.owner.is_some()).count(),engine.entries.values().filter(|e|e.agent.harness==h&&e.agent.state.active()).count(),engine.entries.values().filter(|e|e.agent.harness==h&&e.agent.state==shared::AgentState::Unknown).count());
        }
        engine.reconcile(&processes.live);
        let agents = engine.snapshot(&super::super::pricing::Prices::default());
        let codex: Vec<_> = agents
            .iter()
            .filter(|a| a.harness == Harness::Codex)
            .collect();
        println!("Codex details: children={}, named={}, roles={}, encrypted_missions={}, context={}, observed_skills={}, with_usage={}",
            codex.iter().filter(|a| a.parent_id.is_some()).count(),
            codex.iter().filter(|a| a.agent_path.is_some()).count(),
            codex.iter().filter(|a| a.role.is_some()).count(),
            codex.iter().filter(|a| a.prompt_unavailable).count(),
            codex.iter().filter(|a| a.context.is_some()).count(),
            codex.iter().filter(|a| !a.skills.is_empty()).count(),
            codex.iter().filter(|a| a.estimates.iter().any(|e| e.tokens.input + e.tokens.output + e.tokens.cache_read + e.tokens.cache_write > 0)).count());
        for s in statuses {
            println!(
                "{}: installed={}, processes={}, observed={}, active={}",
                s.harness.label(),
                s.installed,
                processes
                    .harnesses
                    .values()
                    .filter(|h| **h == s.harness)
                    .count(),
                engine
                    .entries
                    .values()
                    .filter(|e| e.agent.harness == s.harness)
                    .count(),
                engine
                    .snapshot(&super::super::pricing::Prices::default())
                    .iter()
                    .filter(|a| a.harness == s.harness && a.state.active())
                    .count()
            );
        }
    }
}

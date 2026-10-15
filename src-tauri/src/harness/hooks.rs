//! Hooks d'observation sans sortie ni décision. Aucun prompt n'est écrit sur disque.
use serde_json::{json, Value};
use shared::Harness;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::PathBuf,
    time::Duration,
};
use tokio::{io::AsyncReadExt, sync::mpsc};
const MARKER: &str = "--ember-harness-event";
const LIMIT: usize = 1024 * 1024;
fn uid() -> u32 {
    // SAFETY: getuid ne prend aucun pointeur et n'a pas de précondition.
    unsafe { libc::getuid() }
}
pub fn socket_path() -> PathBuf {
    PathBuf::from(format!("/tmp/io.github.clementreiffers.ember-{}", uid())).join("agents.sock")
}
pub fn collect_if_requested() -> bool {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(MARKER) {
        return false;
    }
    if let Some(h) = args
        .get(2)
        .and_then(|s| Harness::ALL.into_iter().find(|h| h.key() == s))
    {
        let _ = collect(h);
    }
    true
}
fn collect(h: Harness) -> std::io::Result<()> {
    let mut socket = UnixStream::connect(socket_path())?;
    socket.set_write_timeout(Some(Duration::from_millis(150)))?;
    let mut input = vec![];
    std::io::stdin()
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > LIMIT {
        return Ok(());
    }
    if let Ok(mut payload) = serde_json::from_slice::<Value>(&input) {
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "_ember_at".into(),
                json!(chrono::Utc::now().timestamp_millis()),
            );
        }
        // SAFETY: getppid ne prend aucun pointeur ; le PID sera corroboré par le collecteur.
        let parent = unsafe { libc::getppid() } as u32;
        let envelope = json!({"harness":h,"parent":parent,"payload":payload});
        socket.write_all(&serde_json::to_vec(&envelope)?)?;
    }
    Ok(())
}
#[derive(Debug)]
pub struct HookEvent {
    pub harness: Harness,
    pub parent: u32,
    pub payload: Value,
}
pub fn listen(tx: mpsc::Sender<HookEvent>) -> Result<(), String> {
    let path = socket_path();
    let dir = path.parent().ok_or("Socket indisponible")?;
    match fs::create_dir(dir) {
        Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Permissions de socket impossibles")?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("Répertoire de socket indisponible".into()),
    }
    let meta = fs::symlink_metadata(dir).map_err(|_| "Socket indisponible")?;
    if !meta.is_dir() || meta.uid() != uid() || meta.permissions().mode() & 0o077 != 0 {
        return Err("Répertoire de socket non privé".into());
    }
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if !meta.file_type().is_socket() || meta.uid() != uid() {
            return Err("Chemin de socket occupé".into());
        }
        if UnixStream::connect(&path).is_ok() {
            return Err("Le suivi est déjà ouvert par une autre instance d’Ember".into());
        }
        fs::remove_file(&path).map_err(|_| "Ancienne socket inaccessible")?;
    }
    let listener =
        tokio::net::UnixListener::bind(&path).map_err(|_| "Socket de suivi indisponible")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "Permissions de socket impossibles")?;
    tauri::async_runtime::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            if !stream.peer_cred().is_ok_and(|c| c.uid() == uid()) {
                continue;
            }
            // Traitement séquentiel borné : pas de tâches ni de mémoire illimitées.
            let mut bytes = vec![];
            if tokio::time::timeout(
                Duration::from_millis(200),
                stream.take((LIMIT + 1) as u64).read_to_end(&mut bytes),
            )
            .await
            .is_err()
            {
                continue;
            }
            if bytes.len() > LIMIT {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let (Ok(harness), Some(parent)) = (
                    serde_json::from_value(v["harness"].clone()),
                    v["parent"].as_u64().and_then(|n| u32::try_from(n).ok()),
                ) {
                    let _ = tx.try_send(HookEvent {
                        harness,
                        parent,
                        payload: v["payload"].clone(),
                    });
                }
            }
        }
    });
    Ok(())
}

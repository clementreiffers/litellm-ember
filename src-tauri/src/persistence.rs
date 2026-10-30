//! Écriture atomique : le fichier précédent reste intact tant que le remplacement échoue.
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Écriture des paramètres impossible: {0}")]
    Io(#[from] std::io::Error),
    #[error("Sérialisation impossible: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct Prepared {
    temporary: PathBuf,
    destination: PathBuf,
}

impl Prepared {
    pub fn new(path: &Path, value: &impl serde::Serialize) -> Result<Self, Error> {
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let prepared = Self {
            temporary,
            destination: path.to_owned(),
        };
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&prepared.temporary)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        Ok(prepared)
    }

    pub fn commit(self) -> Result<(), Error> {
        std::fs::rename(&self.temporary, &self.destination)?;
        Ok(())
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}

pub fn write(path: &Path, value: &impl serde::Serialize) -> Result<(), Error> {
    Prepared::new(path, value)?.commit()
}

/// Écrit en arrière-plan le dernier instantané reçu ; les versions intermédiaires peuvent
/// être sautées, jamais réordonnées. L'appelant n'attend jamais le disque.
pub struct Writer<T> {
    latest: tokio::sync::watch::Sender<(u64, Option<T>)>,
    #[cfg_attr(not(test), allow(dead_code))]
    written: tokio::sync::watch::Receiver<u64>,
}

impl<T: serde::Serialize + Clone + Send + Sync + 'static> Writer<T> {
    pub fn spawn(path: PathBuf) -> Self {
        let (latest, mut updates) = tokio::sync::watch::channel((0, None::<T>));
        let (done, written) = tokio::sync::watch::channel(0);
        tauri::async_runtime::spawn(async move {
            while updates.changed().await.is_ok() {
                let (version, Some(snapshot)) = updates.borrow_and_update().clone() else {
                    continue;
                };
                let path = path.clone();
                #[cfg(test)]
                tests::hold(&path).await;
                let result =
                    tauri::async_runtime::spawn_blocking(move || write(&path, &snapshot)).await;
                if let Err(error) = result
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
                {
                    eprintln!("Sauvegarde : {error}");
                }
                done.send_replace(version);
            }
        });
        Self { latest, written }
    }

    pub fn save(&self, value: T) {
        self.latest
            .send_modify(|(version, latest)| (*version, *latest) = (*version + 1, Some(value)));
    }

    /// Attend que le dernier instantané envoyé ait été traité (écrit ou journalisé).
    #[cfg(test)]
    pub fn flushed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let target = self.latest.borrow().0;
        let mut written = self.written.clone();
        async move {
            let _ = written.wait_for(|done| *done >= target).await;
        }
    }
}

pub fn load_cache(path: &Path) -> Option<shared::Stats> {
    let mut stats: shared::Stats = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    // Les tokens d'un autre jour ne doivent pas apparaître comme ceux d'aujourd'hui.
    if stats.today_date.as_deref()
        != Some(chrono::Local::now().format("%Y-%m-%d").to_string().as_str())
    {
        stats.today.clear();
    }
    stats.error = None;
    Some(stats)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Dossiers dont les écritures restent suspendues jusqu'à `release`.
    static HELD: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

    pub(crate) fn hold_writes_in(directory: &Path) {
        HELD.lock().unwrap().push(directory.to_owned());
    }
    pub(crate) fn release_writes_in(directory: &Path) {
        HELD.lock().unwrap().retain(|held| held != directory);
    }
    pub(super) async fn hold(path: &Path) {
        while HELD
            .lock()
            .unwrap()
            .iter()
            .any(|held| path.starts_with(held))
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn writer_never_blocks_the_caller_and_keeps_the_latest_snapshot() {
        let directory = std::env::temp_dir().join(format!("writer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("value.json");
        let writer = Writer::<Vec<u32>>::spawn(path.clone());
        // Disque bloqué : chaque sauvegarde rend la main sans attendre l'écriture.
        hold_writes_in(&directory);
        for value in 0..200u32 {
            writer.save(vec![value; 10]);
        }
        tokio::task::yield_now().await;
        assert!(!path.exists());
        release_writes_in(&directory);
        writer.flushed().await;
        let saved: Vec<u32> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, vec![199; 10]);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

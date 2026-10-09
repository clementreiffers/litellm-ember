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
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&prepared.temporary)?;
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

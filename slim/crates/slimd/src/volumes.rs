//! Named volume store: directories under <data>/volumes/<name>/_data, with a
//! small json index for labels/createdAt.

use serde::{Deserialize, Serialize};
use slim_api::volume::Volume;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct VolMeta {
    created: String,
    labels: BTreeMap<String, String>,
}

pub struct VolumeManager {
    root: PathBuf,
    index: Mutex<BTreeMap<String, VolMeta>>,
}

impl VolumeManager {
    pub fn open(root: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let index = std::fs::read(root.join("index.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self {
            root: root.to_path_buf(),
            index: Mutex::new(index),
        })
    }

    fn save(&self, idx: &BTreeMap<String, VolMeta>) {
        if let Ok(b) = serde_json::to_vec_pretty(idx) {
            let _ = std::fs::write(self.root.join("index.json"), b);
        }
    }

    pub fn data_path(&self, name: &str) -> PathBuf {
        self.root.join(name).join("_data")
    }

    pub fn create(&self, name: &str, labels: BTreeMap<String, String>) -> io::Result<Volume> {
        let name = if name.is_empty() {
            slim_net::rand_id()
        } else {
            name.to_string()
        };
        std::fs::create_dir_all(self.data_path(&name))?;
        let mut idx = self.index.lock().unwrap();
        idx.entry(name.clone()).or_insert_with(|| VolMeta {
            created: slim_net::now_rfc3339(),
            labels,
        });
        self.save(&idx);
        Ok(self.to_api(&name, idx.get(&name).unwrap()))
    }

    /// Ensure an anonymous/implicit volume exists (used by -v name:/path).
    pub fn ensure(&self, name: &str) -> io::Result<PathBuf> {
        if !self.index.lock().unwrap().contains_key(name) {
            self.create(name, BTreeMap::new())?;
        } else {
            std::fs::create_dir_all(self.data_path(name))?;
        }
        Ok(self.data_path(name))
    }

    pub fn get(&self, name: &str) -> Option<Volume> {
        self.index
            .lock()
            .unwrap()
            .get(name)
            .map(|m| self.to_api(name, m))
    }

    pub fn list(&self) -> Vec<Volume> {
        let idx = self.index.lock().unwrap();
        idx.iter().map(|(n, m)| self.to_api(n, m)).collect()
    }

    pub fn remove(&self, name: &str, force: bool) -> io::Result<()> {
        let mut idx = self.index.lock().unwrap();
        if idx.remove(name).is_none() && !force {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such volume: {name}"),
            ));
        }
        let _ = std::fs::remove_dir_all(self.root.join(name));
        self.save(&idx);
        Ok(())
    }

    fn to_api(&self, name: &str, m: &VolMeta) -> Volume {
        Volume {
            name: name.to_string(),
            driver: "local".into(),
            mountpoint: self.data_path(name).to_string_lossy().into_owned(),
            created_at: m.created.clone(),
            labels: m.labels.clone(),
            scope: "local".into(),
        }
    }
}

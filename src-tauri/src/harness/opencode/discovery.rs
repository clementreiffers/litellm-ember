//! Métadonnées du service v2 : secret emprunté en mémoire, jamais persisté par Ember.
use super::{endpoint, valid_id};
use crate::harness::local::{home, Processes};
use serde_json::Value;
use shared::Harness;
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

pub struct LocalService {
    pub base: String,
    pub password: String,
    pub opened: BTreeMap<String, u32>,
}

fn read(path: &Path) -> Option<Value> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

pub fn discover(processes: &Processes) -> Option<LocalService> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("opencode");
    let service = read(&state.join("service.json"))?;
    let tabs = read(&state.join("latest/tui/tabs.json")).unwrap_or(Value::Null);
    from_metadata(processes, &service, &tabs)
}

fn from_metadata(processes: &Processes, service: &Value, tabs: &Value) -> Option<LocalService> {
    let pid = u32::try_from(service["pid"].as_u64()?).ok()?;
    let base = endpoint(service["url"].as_str()?).ok()?;
    // Ne transmet le secret qu'au service local effectivement observé.
    if processes.harnesses.get(&pid) != Some(&Harness::Opencode)
        || processes.server_owners.get(&base) != Some(&pid)
    {
        return None;
    }
    let password = service["password"].as_str()?.to_owned();
    let mut opened = BTreeMap::new();
    for (owner, directory) in &processes.directories {
        if *owner == pid || processes.harnesses.get(owner) != Some(&Harness::Opencode) {
            continue;
        }
        let groups = [
            &tabs["cwd"][directory.to_string_lossy().as_ref()]["tabs"],
            &tabs["global"]["tabs"],
        ];
        for group in groups {
            for tab in group.as_array().into_iter().flatten() {
                if let Some(id) = tab["sessionID"].as_str().filter(|id| valid_id(id)) {
                    opened.insert(id.to_owned(), *owner);
                }
            }
        }
    }
    Some(LocalService {
        base,
        password,
        opened,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_live_terminal_directories_supply_open_sessions() {
        let mut processes = Processes::default();
        processes
            .harnesses
            .extend([(10, Harness::Opencode), (11, Harness::Opencode)]);
        processes.servers.insert("http://127.0.0.1:1234".into());
        processes
            .directories
            .extend([(10, "/home".into()), (11, "/project".into())]);
        processes
            .server_owners
            .insert("http://127.0.0.1:1234".into(), 10);
        let service = json!({"pid":10,"url":"http://127.0.0.1:1234","password":"local-secret"});
        let tabs = json!({"cwd": {
            "/project": {"tabs":[{"sessionID":"ses_open"}]},
            "/closed": {"tabs":[{"sessionID":"ses_old"}]},
            "/home": {"tabs":[{"sessionID":"ses_service"}]}
        }});
        let discovered = from_metadata(&processes, &service, &tabs).unwrap();
        assert_eq!(discovered.opened, BTreeMap::from([("ses_open".into(), 11)]));
        processes.harnesses.remove(&11);
        assert!(from_metadata(&processes, &service, &tabs)
            .unwrap()
            .opened
            .is_empty());
        processes.server_owners.clear();
        assert!(from_metadata(&processes, &service, &tabs).is_none());
    }
}

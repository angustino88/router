//! The nginx backend map.
//!
//! nginx maps `$prefix_key` (the `<prefix_key>` of `/t/<prefix_key>/...`) to a backend URL from `backends.map`,
//! which the Router writes from the database (active backends with an `upstream_url`) into the directory shared
//! with the nginx container (`NGINX_MAP_DIR`). The container's `deploy/nginx/entrypoint.sh` notices the change,
//! runs `nginx -t`, reloads, and writes the outcome to `nginx-status.json`. A reload of an unchanged map is
//! requested by creating `reload.request`.

use std::{fs, path::PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

pub const MAP_FILE: &str = "backends.map";
const STATUS_FILE: &str = "nginx-status.json";
const RELOAD_REQUEST: &str = "reload.request";

/// One `prefix_key -> upstream` line of the map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MapEntry {
    pub prefix_key: String,
    pub upstream_url: String,
}

/// `key value;` lines of the map; comments and blank lines are ignored.
pub fn parse_map(text: &str) -> Vec<MapEntry> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim().strip_suffix(';')?;
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(k), Some(v), None) => Some(MapEntry { prefix_key: k.into(), upstream_url: v.into() }),
                _ => None,
            }
        })
        .collect()
}

/// Content of the map for `rows` (`prefix_key`, `upstream_url`), sorted by key.
pub fn render_map(rows: &[(String, String)]) -> String {
    let mut rows = rows.to_vec();
    rows.sort();
    rows.dedup_by(|a, b| a.0 == b.0);
    let mut content = String::from("# Written by the Router from the backend registry. Do not edit.\n");
    for (key, url) in rows {
        content.push_str(&format!("{key} {url};\n"));
    }
    content
}

pub struct NginxMaps {
    dir: PathBuf,
    /// Serialises writers of the map inside this process.
    pub lock: tokio::sync::Mutex<()>,
}

impl NginxMaps {
    pub fn new(dir: &str) -> NginxMaps {
        NginxMaps { dir: PathBuf::from(dir), lock: tokio::sync::Mutex::new(()) }
    }

    pub fn map_file(&self) -> PathBuf {
        self.dir.join(MAP_FILE)
    }

    /// Entries of the map as it is on disk.
    pub fn entries(&self) -> Vec<MapEntry> {
        fs::read_to_string(self.map_file()).map(|t| parse_map(&t)).unwrap_or_default()
    }

    /// Rewrites the map (atomically, and only when the content changes). Result: `{"entries", "changed"}`.
    pub fn write(&self, rows: &[(String, String)]) -> Result<Value, String> {
        let content = render_map(rows);
        let path = self.map_file();
        let changed = fs::read_to_string(&path).map(|old| old != content).unwrap_or(true);
        if changed {
            fs::create_dir_all(&self.dir).map_err(|e| format!("create {}: {e}", self.dir.display()))?;
            let tmp = self.dir.join(format!(".{MAP_FILE}.tmp"));
            fs::write(&tmp, &content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
            fs::rename(&tmp, &path).map_err(|e| format!("rename to {}: {e}", path.display()))?;
        }
        Ok(json!({ "entries": parse_map(&content).len(), "changed": changed }))
    }

    /// Asks the nginx watcher to test and reload even when the map did not change.
    pub fn request_reload(&self) -> Result<(), String> {
        let path = self.dir.join(RELOAD_REQUEST);
        fs::write(&path, b"").map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn reload_pending(&self) -> bool {
        self.dir.join(RELOAD_REQUEST).exists()
    }

    /// Last outcome written by the watcher, `None` until nginx reports.
    pub fn nginx_status(&self) -> Option<Value> {
        serde_json::from_str(&fs::read_to_string(self.dir.join(STATUS_FILE)).ok()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_parse() {
        let rows = vec![("b".to_string(), "https://h2".to_string()), ("a".to_string(), "http://h1:1".to_string())];
        let content = render_map(&rows);
        assert!(content.find("a http://h1:1;").unwrap() < content.find("b https://h2;").unwrap());
        assert_eq!(parse_map(&content).len(), 2);
        assert_eq!(parse_map("# c\nx y; # note\nbad\n").len(), 1);
    }

    #[test]
    fn write_only_when_changed() {
        let dir = std::env::temp_dir().join(format!("router-maps-{}", uuid::Uuid::new_v4()));
        let maps = NginxMaps::new(dir.to_str().unwrap());
        let rows = vec![("k".to_string(), "http://h:1".to_string())];
        assert_eq!(maps.write(&rows).unwrap()["changed"], true);
        assert_eq!(maps.write(&rows).unwrap()["changed"], false);
        assert_eq!(maps.entries(), vec![MapEntry { prefix_key: "k".into(), upstream_url: "http://h:1".into() }]);
        assert!(!maps.reload_pending());
        maps.request_reload().unwrap();
        assert!(maps.reload_pending());
        let _ = fs::remove_dir_all(dir);
    }
}

//! 代理池管理器
//!
//! 代理池独立持久化到 `proxies.json`。凭证只保存 `proxyId` 引用，运行时再把真实
//! proxyUrl/账密回填到请求用的凭证副本，避免代理池 URL 散落进 credentials.json。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::http_client::ProxyConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProxyEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ProxyEntry {
    pub fn default_proxies_path() -> &'static str {
        "proxies.json"
    }

    pub fn to_proxy_config(&self) -> ProxyConfig {
        let mut config = ProxyConfig::new(&self.url);
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            config = config.with_auth(username, password);
        }
        config
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProxyHealth {
    pub consecutive_failures: u32,
    pub dead: bool,
    pub last_checked: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

struct ProxyState {
    entry: ProxyEntry,
    health: ProxyHealth,
}

pub const PROXY_DEAD_THRESHOLD: u32 = 3;

pub struct ProxyManager {
    states: Mutex<Vec<ProxyState>>,
    semaphores: Mutex<HashMap<u64, Arc<Semaphore>>>,
    proxies_path: Option<PathBuf>,
}

impl ProxyManager {
    pub fn new(proxies: Vec<ProxyEntry>, proxies_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let max_existing_id = proxies.iter().filter_map(|p| p.id).max().unwrap_or(0);
        let mut next_id = max_existing_id + 1;
        let mut has_new_ids = false;
        let mut seen_ids = std::collections::HashSet::new();
        let mut states = Vec::with_capacity(proxies.len());

        for mut entry in proxies {
            let id = entry.id.unwrap_or_else(|| {
                let id = next_id;
                next_id += 1;
                entry.id = Some(id);
                has_new_ids = true;
                id
            });
            if !seen_ids.insert(id) {
                anyhow::bail!("检测到重复的代理 ID: {}", id);
            }
            states.push(ProxyState {
                entry,
                health: ProxyHealth::default(),
            });
        }

        let manager = Self {
            semaphores: Mutex::new(Self::build_semaphores(&states)),
            states: Mutex::new(states),
            proxies_path,
        };

        if has_new_ids && let Err(error) = manager.persist() {
            tracing::warn!("代理池启动回写失败: {}", error);
        }

        Ok(manager)
    }

    pub fn load_from(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let proxies = Self::read_file(path)?;
        Self::new(proxies, Some(path.to_path_buf()))
    }

    fn read_file(path: &Path) -> anyhow::Result<Vec<ProxyEntry>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&content)?)
    }

    fn build_semaphores(states: &[ProxyState]) -> HashMap<u64, Arc<Semaphore>> {
        states
            .iter()
            .filter_map(|state| {
                let id = state.entry.id?;
                match state.entry.max_concurrency {
                    Some(limit) if limit > 0 => {
                        Some((id, Arc::new(Semaphore::new(limit as usize))))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    fn persist(&self) -> anyhow::Result<bool> {
        let Some(path) = &self.proxies_path else {
            return Ok(false);
        };
        let entries: Vec<ProxyEntry> = {
            let states = self.states.lock();
            states.iter().map(|state| state.entry.clone()).collect()
        };
        let json = serde_json::to_string_pretty(&entries).context("序列化代理池失败")?;
        let real_path = crate::common::io::resolve_symlink_target(path);

        let write = || -> anyhow::Result<()> {
            crate::common::io::atomic_write_string_secure(&real_path, &json)
                .with_context(|| format!("原子写入代理池文件失败: {:?}", real_path))
        };

        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(write)?;
        } else {
            write()?;
        }
        Ok(true)
    }

    pub fn list(&self) -> Vec<ProxyView> {
        let states = self.states.lock();
        let semaphores = self.semaphores.lock();
        states
            .iter()
            .map(|state| {
                let available = state
                    .entry
                    .id
                    .and_then(|id| semaphores.get(&id))
                    .map(|semaphore| semaphore.available_permits());
                ProxyView {
                    entry: state.entry.clone(),
                    health: state.health.clone(),
                    available_permits: available,
                }
            })
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<ProxyEntry> {
        let states = self.states.lock();
        states
            .iter()
            .find(|state| state.entry.id == Some(id))
            .map(|state| state.entry.clone())
    }

    pub fn semaphore_for(&self, id: u64) -> Option<Arc<Semaphore>> {
        self.semaphores.lock().get(&id).cloned()
    }

    pub fn is_usable(&self, id: u64) -> bool {
        let states = self.states.lock();
        states
            .iter()
            .find(|state| state.entry.id == Some(id))
            .map(|state| !state.entry.disabled && !state.health.dead)
            .unwrap_or(false)
    }

    pub fn add(&self, mut entry: ProxyEntry) -> anyhow::Result<u64> {
        let id = {
            let mut states = self.states.lock();
            let next_id = states
                .iter()
                .filter_map(|state| state.entry.id)
                .max()
                .unwrap_or(0)
                + 1;
            entry.id = Some(next_id);
            if let Some(limit) = entry.max_concurrency
                && limit > 0
            {
                self.semaphores
                    .lock()
                    .insert(next_id, Arc::new(Semaphore::new(limit as usize)));
            }
            states.push(ProxyState {
                entry,
                health: ProxyHealth::default(),
            });
            next_id
        };
        self.persist()?;
        Ok(id)
    }

    pub fn update(&self, id: u64, mut entry: ProxyEntry) -> anyhow::Result<()> {
        {
            let mut states = self.states.lock();
            let state = states
                .iter_mut()
                .find(|state| state.entry.id == Some(id))
                .ok_or_else(|| anyhow::anyhow!("代理 #{} 不存在", id))?;
            entry.id = Some(id);
            let limit = entry.max_concurrency;
            state.entry = entry;

            let mut semaphores = self.semaphores.lock();
            match limit {
                Some(limit) if limit > 0 => {
                    semaphores.insert(id, Arc::new(Semaphore::new(limit as usize)));
                }
                _ => {
                    semaphores.remove(&id);
                }
            }
        }
        self.persist()?;
        Ok(())
    }

    pub fn delete(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut states = self.states.lock();
            let before = states.len();
            states.retain(|state| state.entry.id != Some(id));
            if states.len() == before {
                anyhow::bail!("代理 #{} 不存在", id);
            }
            self.semaphores.lock().remove(&id);
        }
        self.persist()?;
        Ok(())
    }

    pub fn set_geo(
        &self,
        id: u64,
        region: Option<String>,
        country: Option<String>,
    ) -> anyhow::Result<()> {
        {
            let mut states = self.states.lock();
            let state = states
                .iter_mut()
                .find(|state| state.entry.id == Some(id))
                .ok_or_else(|| anyhow::anyhow!("代理 #{} 不存在", id))?;
            if region.is_some() {
                state.entry.region = region;
            }
            if country.is_some() {
                state.entry.country = country;
            }
        }
        self.persist()?;
        Ok(())
    }

    pub fn record_health(&self, id: u64, ok: bool, error: Option<String>) -> (bool, bool) {
        let mut states = self.states.lock();
        let Some(state) = states.iter_mut().find(|state| state.entry.id == Some(id)) else {
            return (false, false);
        };
        state.health.last_checked = Some(Utc::now());
        let was_dead = state.health.dead;
        if ok {
            state.health.consecutive_failures = 0;
            state.health.last_error = None;
            state.health.dead = false;
            (false, was_dead)
        } else {
            state.health.consecutive_failures += 1;
            state.health.last_error = error;
            let now_dead = state.health.consecutive_failures >= PROXY_DEAD_THRESHOLD;
            state.health.dead = now_dead;
            (now_dead && !was_dead, false)
        }
    }

    pub fn enabled_ids(&self) -> Vec<u64> {
        let states = self.states.lock();
        states
            .iter()
            .filter(|state| !state.entry.disabled)
            .filter_map(|state| state.entry.id)
            .collect()
    }

    pub fn pick_usable_for_region(&self, region: Option<&str>) -> Option<u64> {
        let states = self.states.lock();
        let semaphores = self.semaphores.lock();
        states
            .iter()
            .filter(|state| !state.entry.disabled && !state.health.dead)
            .filter(|state| match region {
                Some(region) => state.entry.region.as_deref() == Some(region),
                None => true,
            })
            .max_by_key(|state| {
                state
                    .entry
                    .id
                    .and_then(|id| semaphores.get(&id))
                    .map(|semaphore| semaphore.available_permits())
                    .unwrap_or(usize::MAX)
            })
            .and_then(|state| state.entry.id)
    }
}

#[derive(Debug, Clone)]
pub struct ProxyView {
    pub entry: ProxyEntry,
    pub health: ProxyHealth,
    pub available_permits: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str, region: Option<&str>, max_concurrency: Option<u32>) -> ProxyEntry {
        ProxyEntry {
            url: url.to_string(),
            region: region.map(str::to_string),
            max_concurrency,
            ..Default::default()
        }
    }

    #[test]
    fn new_assigns_incremental_ids() {
        let manager = ProxyManager::new(
            vec![entry("http://a", None, None), entry("http://b", None, None)],
            None,
        )
        .unwrap();
        let list = manager.list();
        assert_eq!(list[0].entry.id, Some(1));
        assert_eq!(list[1].entry.id, Some(2));
    }

    #[test]
    fn semaphore_only_for_positive_limit() {
        let manager = ProxyManager::new(
            vec![
                entry("http://limited", None, Some(2)),
                entry("http://unlimited", None, None),
                entry("http://zero", None, Some(0)),
            ],
            None,
        )
        .unwrap();
        assert_eq!(manager.semaphore_for(1).unwrap().available_permits(), 2);
        assert!(manager.semaphore_for(2).is_none());
        assert!(manager.semaphore_for(3).is_none());
    }

    #[test]
    fn health_marks_dead_after_threshold_and_recovers() {
        let manager = ProxyManager::new(vec![entry("http://x", None, None)], None).unwrap();
        for _ in 0..PROXY_DEAD_THRESHOLD {
            manager.record_health(1, false, Some("timeout".to_string()));
        }
        assert!(!manager.is_usable(1));
        let (_, recovered) = manager.record_health(1, true, None);
        assert!(recovered);
        assert!(manager.is_usable(1));
    }

    #[test]
    fn pick_usable_respects_region_and_health() {
        let manager = ProxyManager::new(
            vec![
                entry("http://us1", Some("us"), None),
                entry("http://eu1", Some("eu"), None),
            ],
            None,
        )
        .unwrap();
        assert_eq!(manager.pick_usable_for_region(Some("eu")), Some(2));
        assert_eq!(manager.pick_usable_for_region(Some("us")), Some(1));
        assert_eq!(manager.pick_usable_for_region(Some("ap")), None);
    }
}

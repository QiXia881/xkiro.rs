//! Session 亲和性管理
//!
//! 用 session_id（per-conversation UUID）绑定凭据，让同一会话连续请求
//! 黏住同一凭据，提高上游 prompt cache 命中率。
//!
//! 不能用 device-level 的 user_id（机器哈希常量），那样会让所有 session
//! 永久粘连同一凭据，破坏多凭据平摊。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);
/// 单 session map 上限，防长期跑导致 map 无限增长（保护性硬上限）
const MAX_ENTRIES: usize = 4096;

struct AffinityEntry {
    credential_id: u64,
    last_used: Instant,
}

pub struct SessionAffinity {
    inner: Mutex<HashMap<String, AffinityEntry>>,
    ttl: Duration,
}

impl Default for SessionAffinity {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

impl SessionAffinity {
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// 获取 session 绑定的凭据 id；过期则惰性清理
    pub fn get(&self, session_id: &str) -> Option<u64> {
        let mut map = self.inner.lock();
        if let Some(entry) = map.get(session_id) {
            if entry.last_used.elapsed() < self.ttl {
                return Some(entry.credential_id);
            }
            map.remove(session_id);
        }
        None
    }

    /// 建立 / 覆盖绑定
    pub fn set(&self, session_id: &str, credential_id: u64) {
        let mut map = self.inner.lock();
        if map.len() >= MAX_ENTRIES && !map.contains_key(session_id) {
            self.evict_oldest(&mut map);
        }
        map.insert(
            session_id.to_string(),
            AffinityEntry {
                credential_id,
                last_used: Instant::now(),
            },
        );
    }

    /// 续期 last_used（命中且复用时调用）
    pub fn touch(&self, session_id: &str) {
        let mut map = self.inner.lock();
        if let Some(entry) = map.get_mut(session_id) {
            entry.last_used = Instant::now();
        }
    }

    /// 移除特定 session 的绑定（凭据不再可用时调用）
    pub fn remove(&self, session_id: &str) {
        self.inner.lock().remove(session_id);
    }

    /// 移除所有绑到指定凭据的 session（凭据被禁用 / 删除时）
    pub fn remove_by_credential(&self, credential_id: u64) {
        self.inner
            .lock()
            .retain(|_, entry| entry.credential_id != credential_id);
    }

    /// 清空全部绑定（关闭 affinity 开关时调用）
    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// 清理过期条目
    #[allow(dead_code)]
    pub fn cleanup(&self) {
        let mut map = self.inner.lock();
        let ttl = self.ttl;
        map.retain(|_, entry| entry.last_used.elapsed() < ttl);
    }

    /// 当前活跃 session 数（含未过期）
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    fn evict_oldest(&self, map: &mut HashMap<String, AffinityEntry>) {
        if let Some((oldest_key, _)) = map
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, e)| (k.clone(), e.last_used))
        {
            map.remove(&oldest_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_set_touch() {
        let aff = SessionAffinity::new(Duration::from_secs(60));
        assert_eq!(aff.get("s1"), None);
        aff.set("s1", 7);
        assert_eq!(aff.get("s1"), Some(7));
        aff.touch("s1");
        assert_eq!(aff.get("s1"), Some(7));
    }

    #[test]
    fn ttl_expires() {
        let aff = SessionAffinity::new(Duration::from_millis(20));
        aff.set("s1", 1);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(aff.get("s1"), None);
    }

    #[test]
    fn remove_by_credential() {
        let aff = SessionAffinity::new(Duration::from_secs(60));
        aff.set("s1", 1);
        aff.set("s2", 1);
        aff.set("s3", 2);
        aff.remove_by_credential(1);
        assert_eq!(aff.get("s1"), None);
        assert_eq!(aff.get("s2"), None);
        assert_eq!(aff.get("s3"), Some(2));
    }

    #[test]
    fn evict_when_full() {
        let aff = SessionAffinity::new(Duration::from_secs(60));
        // 走业务路径插，确保 evict_oldest 在边界触发
        for i in 0..MAX_ENTRIES {
            aff.set(&format!("s{}", i), i as u64);
        }
        assert_eq!(aff.len(), MAX_ENTRIES);
        aff.set("new", 999);
        assert_eq!(aff.len(), MAX_ENTRIES);
        assert_eq!(aff.get("new"), Some(999));
    }
}

//! 凭据调度亲和性管理
//!
//! 用稳定 routing key 绑定凭据，让同一类请求连续回到同一凭据。
//!
//! 调用方负责选择 key 的语义：会话亲和必须使用 per-conversation UUID，
//! 客户端亲和使用已认证的客户端 API 密钥 ID，二者不能混用同一个 map。

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

pub struct CredentialAffinity {
    inner: Mutex<HashMap<String, AffinityEntry>>,
    ttl: Duration,
}

impl Default for CredentialAffinity {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

impl CredentialAffinity {
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// 获取 routing key 绑定的凭据 id；过期则惰性清理
    pub fn get(&self, key: &str) -> Option<u64> {
        let mut map = self.inner.lock();
        if let Some(entry) = map.get(key) {
            if entry.last_used.elapsed() < self.ttl {
                return Some(entry.credential_id);
            }
            map.remove(key);
        }
        None
    }

    /// 建立 / 覆盖绑定
    pub fn set(&self, key: &str, credential_id: u64) {
        let mut map = self.inner.lock();
        if map.len() >= MAX_ENTRIES && !map.contains_key(key) {
            self.evict_oldest(&mut map);
        }
        map.insert(
            key.to_string(),
            AffinityEntry {
                credential_id,
                last_used: Instant::now(),
            },
        );
    }

    /// 续期 last_used（命中且复用时调用）
    pub fn touch(&self, key: &str) {
        let mut map = self.inner.lock();
        if let Some(entry) = map.get_mut(key) {
            entry.last_used = Instant::now();
        }
    }

    /// 移除特定 routing key 的绑定（凭据不再可用时调用）
    pub fn remove(&self, key: &str) {
        self.inner.lock().remove(key);
    }

    /// 移除所有绑到指定凭据的 routing key（凭据被禁用 / 删除时）
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

    /// 当前活跃 routing key 数（含未过期）
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
        let aff = CredentialAffinity::new(Duration::from_secs(60));
        assert_eq!(aff.get("s1"), None);
        aff.set("s1", 7);
        assert_eq!(aff.get("s1"), Some(7));
        aff.touch("s1");
        assert_eq!(aff.get("s1"), Some(7));
    }

    #[test]
    fn ttl_expires() {
        let aff = CredentialAffinity::new(Duration::from_millis(20));
        aff.set("s1", 1);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(aff.get("s1"), None);
    }

    #[test]
    fn remove_by_credential() {
        let aff = CredentialAffinity::new(Duration::from_secs(60));
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
        let aff = CredentialAffinity::new(Duration::from_secs(60));
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

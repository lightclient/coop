use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 100;

pub(crate) struct Cache<V> {
    entries: HashMap<String, CacheEntry<V>>,
    insertion_order: Vec<String>,
}

struct CacheEntry<V> {
    value: V,
    expires_at: Instant,
}

impl<V: Clone> Cache<V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: Vec::new(),
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        let normalized = key.to_lowercase();
        let entry = self.entries.get(&normalized)?;
        (Instant::now() < entry.expires_at).then_some(&entry.value)
    }

    pub(crate) fn insert(&mut self, key: &str, value: V, ttl: Duration) {
        let normalized = key.to_lowercase();

        let now = Instant::now();
        self.entries.retain(|_, e| now < e.expires_at);
        self.insertion_order
            .retain(|k| self.entries.contains_key(k));

        while self.entries.len() >= MAX_ENTRIES {
            if let Some(oldest) = self.insertion_order.first().cloned() {
                self.entries.remove(&oldest);
                self.insertion_order.remove(0);
            } else {
                break;
            }
        }

        if !self.entries.contains_key(&normalized) {
            self.insertion_order.push(normalized.clone());
        }

        self.entries.insert(
            normalized,
            CacheEntry {
                value,
                expires_at: now + ttl,
            },
        );
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let mut cache = Cache::new();
        cache.insert("key1", "value1".to_owned(), Duration::from_secs(60));
        assert_eq!(cache.get("key1"), Some(&"value1".to_owned()));
    }

    #[test]
    fn key_normalization() {
        let mut cache = Cache::new();
        cache.insert("MyKey", "value".to_owned(), Duration::from_secs(60));
        assert_eq!(cache.get("mykey"), Some(&"value".to_owned()));
        assert_eq!(cache.get("MYKEY"), Some(&"value".to_owned()));
    }

    #[test]
    fn expired_entry_returns_none() {
        let mut cache = Cache::new();
        cache.insert("key", "value".to_owned(), Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(cache.get("key"), None);
    }

    #[test]
    fn capacity_eviction() {
        let mut cache = Cache::new();
        for i in 0..MAX_ENTRIES {
            cache.insert(&format!("key{i}"), i, Duration::from_secs(60));
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);

        cache.insert("overflow", 999, Duration::from_secs(60));
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.get("key0").is_none());
        assert_eq!(cache.get("overflow"), Some(&999));
    }

    #[test]
    fn overwrite_existing_key() {
        let mut cache = Cache::new();
        cache.insert("key", "v1".to_owned(), Duration::from_secs(60));
        cache.insert("key", "v2".to_owned(), Duration::from_secs(60));
        assert_eq!(cache.get("key"), Some(&"v2".to_owned()));
        assert_eq!(cache.entries.len(), 1);
    }
}

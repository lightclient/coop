//! API key pool with automatic rotation based on rate-limit headers.

use reqwest::header::HeaderMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tracing::debug;

const NEAR_LIMIT_THRESHOLD: f64 = 0.90;

/// Per-key rate-limit state, updated from Anthropic response headers.
#[derive(Debug)]
struct RateLimitInfo {
    allowed: bool,
    utilization: Option<f64>,
    representative_claim: Option<String>,
    reset_epoch: Option<u64>,
    cooldown_until: Option<Instant>,
}

impl Default for RateLimitInfo {
    fn default() -> Self {
        Self {
            allowed: true,
            utilization: None,
            representative_claim: None,
            reset_epoch: None,
            cooldown_until: None,
        }
    }
}

struct KeyEntry {
    value: String,
    is_oauth: bool,
    rate_limits: RwLock<RateLimitInfo>,
}

pub struct KeyPool {
    keys: Vec<KeyEntry>,
}

impl std::fmt::Debug for KeyPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPool")
            .field("key_count", &self.keys.len())
            .finish()
    }
}

impl KeyPool {
    pub fn new(api_keys: Vec<String>) -> Self {
        let keys = api_keys
            .into_iter()
            .map(|value| {
                let is_oauth = value.contains("sk-ant-oat");
                KeyEntry {
                    value,
                    is_oauth,
                    rate_limits: RwLock::new(RateLimitInfo::default()),
                }
            })
            .collect();
        Self { keys }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn get(&self, index: usize) -> (&str, bool) {
        let entry = &self.keys[index];
        (&entry.value, entry.is_oauth)
    }

    /// Pick the best key for the next request. Always returns a valid index.
    pub fn best_key(&self) -> usize {
        if self.keys.len() == 1 {
            return 0;
        }

        let now = Instant::now();

        let mut comfortable: Vec<(usize, u64)> = Vec::new();
        let mut hot: Vec<(usize, f64, u64)> = Vec::new();
        let mut cooldown: Vec<(usize, Instant)> = Vec::new();

        for (i, entry) in self.keys.iter().enumerate() {
            let info = entry.rate_limits.read().expect("rate_limits lock poisoned");
            let cooldown_until = info.cooldown_until;
            let utilization = info.utilization;
            let reset = info.reset_epoch.unwrap_or(u64::MAX);
            drop(info);

            if let Some(until) = cooldown_until
                && until > now
            {
                cooldown.push((i, until));
                continue;
            }

            match utilization {
                Some(u) if u >= NEAR_LIMIT_THRESHOLD => {
                    hot.push((i, u, reset));
                }
                _ => {
                    comfortable.push((i, reset));
                }
            }
        }

        if !comfortable.is_empty() {
            // Pick the one whose reset_epoch is soonest (closest to fresh capacity).
            comfortable.sort_by_key(|&(_, reset)| reset);
            return comfortable[0].0;
        }

        if !hot.is_empty() {
            // All non-cooldown keys are hot: pick lowest utilization, tiebreak by soonest reset.
            hot.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.2.cmp(&b.2))
            });
            return hot[0].0;
        }

        // All on cooldown: pick soonest cooldown_until.
        cooldown.sort_by_key(|&(_, until)| until);
        cooldown.first().map_or(0, |&(i, _)| i)
    }

    /// Update rate-limit info from Anthropic response headers.
    pub fn update_from_headers(&self, key_index: usize, headers: &HeaderMap) {
        let entry = &self.keys[key_index];
        let mut info = entry
            .rate_limits
            .write()
            .expect("rate_limits lock poisoned");

        if let Some(status) = header_str(headers, "anthropic-ratelimit-unified-status") {
            info.allowed = status == "allowed";
        }

        if let Some(reset) = header_str(headers, "anthropic-ratelimit-unified-reset")
            && let Ok(epoch) = reset.parse::<u64>()
        {
            info.reset_epoch = Some(epoch);
        }

        if let Some(claim) = header_str(headers, "anthropic-ratelimit-unified-representative-claim")
        {
            info.representative_claim = Some(claim.to_owned());
        }

        // Read utilization for the representative claim window.
        if let Some(ref claim) = info.representative_claim
            && let Some(util) = read_utilization_for_claim(headers, claim)
        {
            debug!(
                key_index,
                utilization = util,
                claim = claim.as_str(),
                "rate-limit utilization updated"
            );
            info.utilization = Some(util);
        }

        if let Some(retry_after) = header_str(headers, "retry-after")
            && let Ok(secs) = retry_after.parse::<u64>()
        {
            info.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    pub fn mark_rate_limited(&self, key_index: usize, retry_after_secs: u64) {
        let entry = &self.keys[key_index];
        let mut info = entry
            .rate_limits
            .write()
            .expect("rate_limits lock poisoned");
        info.cooldown_until = Some(Instant::now() + Duration::from_secs(retry_after_secs));
        info.allowed = false;
    }

    pub fn is_near_limit(&self, key_index: usize) -> bool {
        let entry = &self.keys[key_index];
        let info = entry.rate_limits.read().expect("rate_limits lock poisoned");
        info.utilization.is_some_and(|u| u >= NEAR_LIMIT_THRESHOLD)
    }

    pub fn on_cooldown(&self, key_index: usize) -> bool {
        let entry = &self.keys[key_index];
        let info = entry.rate_limits.read().expect("rate_limits lock poisoned");
        info.cooldown_until
            .is_some_and(|until| until > Instant::now())
    }

    pub fn utilization(&self, key_index: usize) -> Option<f64> {
        let entry = &self.keys[key_index];
        let info = entry.rate_limits.read().expect("rate_limits lock poisoned");
        info.utilization
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// Map a representative claim name to the utilization header value.
fn read_utilization_for_claim(headers: &HeaderMap, claim: &str) -> Option<f64> {
    let header_name = match claim {
        "five_hour" => "anthropic-ratelimit-unified-5h-utilization".to_owned(),
        "seven_day" => "anthropic-ratelimit-unified-7d-utilization".to_owned(),
        other => {
            // Model-specific windows like "seven_day_sonnet" -> "7d_sonnet"
            let mapped = other.replace("seven_day", "7d").replace("five_hour", "5h");
            format!("anthropic-ratelimit-unified-{mapped}-utilization")
        }
    };

    if let Some(val) = header_str(headers, &header_name)
        && let Ok(u) = val.parse::<f64>()
    {
        return Some(u);
    }

    // Fallback: find highest utilization across all windows.
    let mut max_util: Option<f64> = None;
    for (name, value) in headers {
        let name_str = name.as_str();
        if name_str.starts_with("anthropic-ratelimit-unified-")
            && name_str.ends_with("-utilization")
            && let Ok(u) = value.to_str().unwrap_or("").parse::<f64>()
        {
            max_util = Some(max_util.map_or(u, |m: f64| m.max(u)));
        }
    }
    max_util
}

/// Parse `env:VAR_NAME` key references and resolve them.
pub fn resolve_key_refs(key_refs: &[String]) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::with_capacity(key_refs.len());
    for entry in key_refs {
        if let Some(var_name) = entry.strip_prefix("env:") {
            let value = std::env::var(var_name).map_err(|_env_err| {
                anyhow::anyhow!(
                    "environment variable '{var_name}' not set (from api_keys entry '{entry}')"
                )
            })?;
            keys.push(value);
        } else {
            anyhow::bail!(
                "api_keys entry '{entry}' must use 'env:' prefix (e.g. env:ANTHROPIC_API_KEY)"
            );
        }
    }
    Ok(keys)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn pool_with_keys(n: usize) -> KeyPool {
        let keys: Vec<String> = (0..n).map(|i| format!("sk-ant-api-test-{i}")).collect();
        KeyPool::new(keys)
    }

    fn set_utilization(pool: &KeyPool, idx: usize, util: f64) {
        let entry = &pool.keys[idx];
        let mut info = entry.rate_limits.write().unwrap();
        info.utilization = Some(util);
    }

    fn set_reset_epoch(pool: &KeyPool, idx: usize, epoch: u64) {
        let entry = &pool.keys[idx];
        let mut info = entry.rate_limits.write().unwrap();
        info.reset_epoch = Some(epoch);
    }

    fn set_cooldown(pool: &KeyPool, idx: usize, duration: Duration) {
        let entry = &pool.keys[idx];
        let mut info = entry.rate_limits.write().unwrap();
        info.cooldown_until = Some(Instant::now() + duration);
    }

    #[test]
    fn single_key_pool() {
        let pool = pool_with_keys(1);
        assert_eq!(pool.best_key(), 0);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn prefers_soonest_reset_among_comfortable_keys() {
        let pool = pool_with_keys(3);
        set_utilization(&pool, 0, 0.50);
        set_utilization(&pool, 1, 0.30);
        set_utilization(&pool, 2, 0.40);
        set_reset_epoch(&pool, 0, 3000);
        set_reset_epoch(&pool, 1, 1000); // soonest
        set_reset_epoch(&pool, 2, 2000);

        assert_eq!(pool.best_key(), 1);
    }

    #[test]
    fn skips_near_limit_keys() {
        let pool = pool_with_keys(2);
        set_utilization(&pool, 0, 0.95);
        set_utilization(&pool, 1, 0.50);

        assert_eq!(pool.best_key(), 1);
    }

    #[test]
    fn skips_cooldown_keys() {
        let pool = pool_with_keys(2);
        set_cooldown(&pool, 0, Duration::from_secs(60));
        // key 1 is fine

        assert_eq!(pool.best_key(), 1);
    }

    #[test]
    fn all_keys_hot_picks_lowest_utilization() {
        let pool = pool_with_keys(2);
        set_utilization(&pool, 0, 0.92);
        set_utilization(&pool, 1, 0.95);

        assert_eq!(pool.best_key(), 0);
    }

    #[test]
    fn all_keys_hot_tiebreak_by_soonest_reset() {
        let pool = pool_with_keys(2);
        set_utilization(&pool, 0, 0.92);
        set_utilization(&pool, 1, 0.92);
        set_reset_epoch(&pool, 0, 2000);
        set_reset_epoch(&pool, 1, 1000); // soonest

        assert_eq!(pool.best_key(), 1);
    }

    #[test]
    fn all_keys_on_cooldown_picks_soonest() {
        let pool = pool_with_keys(2);
        set_cooldown(&pool, 0, Duration::from_secs(60));
        set_cooldown(&pool, 1, Duration::from_secs(10)); // soonest

        assert_eq!(pool.best_key(), 1);
    }

    #[test]
    fn cooldown_expires() {
        let pool = pool_with_keys(1);
        set_cooldown(&pool, 0, Duration::from_secs(0));
        // 0-second cooldown should already be expired
        assert!(!pool.on_cooldown(0));
    }

    #[test]
    fn update_from_headers_parses_unified_headers() {
        let pool = pool_with_keys(1);
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-status"),
            HeaderValue::from_static("allowed"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-reset"),
            HeaderValue::from_static("1770685200"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-representative-claim"),
            HeaderValue::from_static("five_hour"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-5h-utilization"),
            HeaderValue::from_static("0.12"),
        );

        pool.update_from_headers(0, &headers);

        let info = pool.keys[0].rate_limits.read().unwrap();
        let allowed = info.allowed;
        let reset_epoch = info.reset_epoch;
        let claim = info.representative_claim.clone();
        let util = info.utilization;
        drop(info);

        assert!(allowed);
        assert_eq!(reset_epoch, Some(1_770_685_200));
        assert_eq!(claim.as_deref(), Some("five_hour"));
        assert!((util.unwrap() - 0.12).abs() < f64::EPSILON);
    }

    #[test]
    fn update_from_headers_maps_representative_claim_to_utilization() {
        let pool = pool_with_keys(1);
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-representative-claim"),
            HeaderValue::from_static("seven_day"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-7d-utilization"),
            HeaderValue::from_static("0.45"),
        );

        pool.update_from_headers(0, &headers);

        let util = pool.utilization(0).unwrap();
        assert!((util - 0.45).abs() < f64::EPSILON);
    }

    #[test]
    fn update_from_headers_retry_after_sets_cooldown() {
        let pool = pool_with_keys(1);
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("retry-after"),
            HeaderValue::from_static("30"),
        );

        pool.update_from_headers(0, &headers);

        assert!(pool.on_cooldown(0));
    }

    #[test]
    fn update_from_headers_ignores_missing() {
        let pool = pool_with_keys(1);
        set_utilization(&pool, 0, 0.50);
        set_reset_epoch(&pool, 0, 1000);

        let headers = HeaderMap::new(); // empty
        pool.update_from_headers(0, &headers);

        let util = pool.utilization(0).unwrap();
        assert!((util - 0.50).abs() < f64::EPSILON);
        let info = pool.keys[0].rate_limits.read().unwrap();
        let reset_epoch = info.reset_epoch;
        drop(info);
        assert_eq!(reset_epoch, Some(1000));
    }

    #[test]
    fn is_near_limit_thresholds() {
        let pool = pool_with_keys(1);

        set_utilization(&pool, 0, 0.89);
        assert!(!pool.is_near_limit(0));

        set_utilization(&pool, 0, 0.90);
        assert!(pool.is_near_limit(0));

        set_utilization(&pool, 0, 1.0);
        assert!(pool.is_near_limit(0));
    }

    #[test]
    fn unknown_utilization_treated_as_comfortable() {
        let pool = pool_with_keys(2);
        // key 0: fresh, no utilization known
        // key 1: at 95%
        set_utilization(&pool, 1, 0.95);

        assert_eq!(pool.best_key(), 0);
    }

    #[test]
    fn oauth_detection() {
        let pool = KeyPool::new(vec![
            "sk-ant-oat01-test".to_owned(),
            "sk-ant-api01-test".to_owned(),
        ]);
        assert!(pool.get(0).1); // OAuth
        assert!(!pool.get(1).1); // standard
    }

    #[test]
    fn resolve_key_refs_parses_env_prefix() {
        // Use HOME which is always set in CI/test environments.
        let result = resolve_key_refs(&["env:HOME".to_owned()]);
        assert!(result.is_ok());
        let keys = result.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(!keys[0].is_empty());
    }

    #[test]
    fn resolve_key_refs_rejects_unknown_prefix() {
        let result = resolve_key_refs(&["vault:secret".to_owned()]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("env:"),
            "error should suggest env: prefix: {err}"
        );
    }

    #[test]
    fn resolve_key_refs_rejects_bare_names() {
        let result = resolve_key_refs(&["ANTHROPIC_API_KEY".to_owned()]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("env:"),
            "error should suggest env: prefix: {err}"
        );
    }

    #[test]
    fn resolve_key_refs_reports_missing_env_var() {
        let result = resolve_key_refs(&["env:MISSING_COOP_TEST_KEY_99".to_owned()]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("MISSING_COOP_TEST_KEY_99"));
    }
}

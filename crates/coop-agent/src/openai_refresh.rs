use std::sync::RwLock;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use tracing::{debug, info};

use crate::openai_codex;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REFRESH_BUFFER_MS: i64 = 5 * 60 * 1000;

pub(crate) struct FreshToken {
    pub access_token: String,
    pub account_id: String,
}

struct TokenState {
    access_token: String,
    account_id: String,
    refresh_token: String,
    expires_at_ms: i64,
}

pub(crate) struct RefreshState {
    state: RwLock<TokenState>,
    lock: tokio::sync::Mutex<()>,
}

impl RefreshState {
    pub(crate) fn new(access_token: String, account_id: String, refresh_token: String) -> Self {
        let expires_at_ms = openai_codex::jwt_expires_at_ms(&access_token).unwrap_or(0);
        Self {
            state: RwLock::new(TokenState {
                access_token,
                account_id,
                refresh_token,
                expires_at_ms,
            }),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) async fn ensure_fresh(&self, client: &Client) -> Result<FreshToken> {
        {
            let state = self.state.read().expect("refresh state poisoned");
            if now_ms() < state.expires_at_ms - REFRESH_BUFFER_MS {
                return Ok(FreshToken {
                    access_token: state.access_token.clone(),
                    account_id: state.account_id.clone(),
                });
            }
        }

        let _guard = self.lock.lock().await;

        {
            let state = self.state.read().expect("refresh state poisoned");
            if now_ms() < state.expires_at_ms - REFRESH_BUFFER_MS {
                return Ok(FreshToken {
                    access_token: state.access_token.clone(),
                    account_id: state.account_id.clone(),
                });
            }
        }

        let old_refresh = self
            .state
            .read()
            .expect("refresh state poisoned")
            .refresh_token
            .clone();

        info!("refreshing OpenAI Codex access token");
        let result = do_refresh(client, &old_refresh).await?;

        let account_id =
            openai_codex::extract_account_id(&result.access_token).unwrap_or_else(|| {
                self.state
                    .read()
                    .expect("refresh state poisoned")
                    .account_id
                    .clone()
            });
        let expires_at_ms = openai_codex::jwt_expires_at_ms(&result.access_token)
            .unwrap_or_else(|| now_ms() + result.expires_in_ms);

        {
            let mut state = self.state.write().expect("refresh state poisoned");
            state.access_token.clone_from(&result.access_token);
            state.account_id.clone_from(&account_id);
            state.refresh_token = result.refresh_token;
            state.expires_at_ms = expires_at_ms;
        }

        debug!("OpenAI Codex access token refreshed");
        Ok(FreshToken {
            access_token: result.access_token,
            account_id,
        })
    }
}

struct RefreshResult {
    access_token: String,
    refresh_token: String,
    expires_in_ms: i64,
}

async fn do_refresh(client: &Client, refresh_token: &str) -> Result<RefreshResult> {
    let response = client
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .context("failed to send OpenAI token refresh request")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "OpenAI token refresh failed (HTTP {}): {}",
            status.as_u16(),
            body.trim()
        );
    }

    let body: Value = response
        .json()
        .await
        .context("failed to parse OpenAI token refresh response")?;

    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("refresh response missing access_token"))?
        .to_owned();
    let new_refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("refresh response missing refresh_token"))?
        .to_owned();
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("refresh response missing expires_in"))?;

    Ok(RefreshResult {
        access_token,
        refresh_token: new_refresh,
        expires_in_ms: expires_in * 1000,
    })
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_millis(),
    )
    .expect("system time exceeds i64 range")
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    fn fake_jwt(account_id: &str, exp_secs: i64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id,
                },
                "exp": exp_secs,
            }))
            .unwrap(),
        );
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn fresh_token_not_refreshed() {
        let future = now_ms() / 1000 + 3600;
        let token = fake_jwt("acct_1", future);
        let rs = RefreshState::new(token, "acct_1".into(), "refresh_tok".into());
        let expires = rs.state.read().unwrap().expires_at_ms;
        assert!(now_ms() < expires - REFRESH_BUFFER_MS);
    }

    #[test]
    fn expired_token_needs_refresh() {
        let past = now_ms() / 1000 - 60;
        let token = fake_jwt("acct_1", past);
        let rs = RefreshState::new(token, "acct_1".into(), "refresh_tok".into());
        let expires = rs.state.read().unwrap().expires_at_ms;
        assert!(now_ms() >= expires - REFRESH_BUFFER_MS);
    }
}

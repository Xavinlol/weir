use thiserror::Error;

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum WeirError {
    #[error("failed to forward request to Discord: {0}")]
    DiscordRequest(#[from] reqwest::Error),

    #[error("rate limited: retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },

    #[error("token disabled due to suspected ban: {bot_id}")]
    TokenDisabled { bot_id: String },

    #[error("request timeout after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },

    #[error("invalid authorization header")]
    InvalidAuth,

    #[error("internal error: {0}")]
    Internal(String),
}

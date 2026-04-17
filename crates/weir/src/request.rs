use base64::engine::general_purpose::STANDARD;
use base64::Engine;

#[derive(Debug, Clone)]
pub enum Auth {
    Bot {
        bot_id: String,
    },
    Bearer {
        bot_id: String,
    },
    #[allow(dead_code)]
    Webhook {
        webhook_id: String,
    },
    None,
}

impl Auth {
    pub fn from_header(value: &str) -> Self {
        let value = value.trim();

        if let Some(token) = value
            .strip_prefix("Bot ")
            .or_else(|| value.strip_prefix("bot "))
        {
            let bot_id = extract_bot_id(token).unwrap_or_default();
            Self::Bot { bot_id }
        } else if let Some(token) = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
        {
            let bot_id = extract_bot_id(token).unwrap_or_default();
            Self::Bearer { bot_id }
        } else {
            Self::None
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub fn rate_limit_key(&self) -> Option<&str> {
        match self {
            Self::Bot { bot_id } | Self::Bearer { bot_id } => Some(bot_id),
            Self::Webhook { webhook_id } => Some(webhook_id),
            Self::None => None,
        }
    }
}

fn extract_bot_id(token: &str) -> Option<String> {
    let first_segment = token.split('.').next()?;
    let decoded = STANDARD.decode(first_segment).ok()?;
    let id_str = String::from_utf8(decoded).ok()?;

    if !id_str.is_empty() && id_str.bytes().all(|b| b.is_ascii_digit()) {
        Some(id_str)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bot_token() {
        let auth = Auth::from_header("Bot MTIzNDU2Nzg5.Zm9v.YmFy");
        match auth {
            Auth::Bot { bot_id } => assert_eq!(bot_id, "123456789"),
            other => panic!("expected Bot, got {other:?}"),
        }
    }

    #[test]
    fn parse_bearer_token() {
        let auth = Auth::from_header("Bearer MTIzNDU2Nzg5.Zm9v.YmFy");
        match auth {
            Auth::Bearer { bot_id } => assert_eq!(bot_id, "123456789"),
            other => panic!("expected Bearer, got {other:?}"),
        }
    }

    #[test]
    fn parse_invalid_header() {
        let auth = Auth::from_header("InvalidScheme token");
        assert!(matches!(auth, Auth::None));
    }

    #[test]
    fn rate_limit_key_for_bot() {
        let auth = Auth::Bot {
            bot_id: "123".to_owned(),
        };
        assert_eq!(auth.rate_limit_key(), Some("123"));
    }
}

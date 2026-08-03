use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    Channels,
    Guilds,
    Webhooks,
    Invites,
    Interactions,
    Unknown,
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Channels => "channels",
            Self::Guilds => "guilds",
            Self::Webhooks => "webhooks",
            Self::Invites => "invites",
            Self::Interactions => "interactions",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubResource {
    Messages,
    Pins,
    Members,
    Bans,
    Reactions,
    ReactionsModify,
}

impl fmt::Display for SubResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Messages => "messages",
            Self::Pins => "pins",
            Self::Members => "members",
            Self::Bans => "bans",
            Self::Reactions => "reactions",
            Self::ReactionsModify => "reactions/!modify",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Other,
}

impl Method {
    pub fn from_http(s: &str) -> Self {
        match s {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Other => "OTHER",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    pub method: Method,
    pub resource: Resource,
    pub major_id: String,
    pub sub_resource: Option<SubResource>,
}

impl BucketKey {
    pub fn is_interaction(&self) -> bool {
        self.resource == Resource::Interactions
    }
}

impl fmt::Display for BucketKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.sub_resource {
            Some(sub) => write!(
                f,
                "{}:{}/{}/{sub}",
                self.method, self.resource, self.major_id
            ),
            None => write!(f, "{}:{}/{}", self.method, self.resource, self.major_id),
        }
    }
}

/// Parse a Discord API path into a bucket key for rate limiting.
#[inline]
pub fn parse_bucket_key(method: &str, path: &str) -> BucketKey {
    let method_enum = Method::from_http(method);
    let path = strip_api_prefix(path.trim_start_matches('/'));
    let (resource_str, rest) = path.split_once('/').unwrap_or((path, ""));
    let (major_id_str, sub_path) = rest.split_once('/').unwrap_or((rest, ""));

    let resource = match resource_str {
        "channels" => Resource::Channels,
        "guilds" => Resource::Guilds,
        "webhooks" => Resource::Webhooks,
        "invites" => Resource::Invites,
        "interactions" => Resource::Interactions,
        _ => Resource::Unknown,
    };

    match resource {
        Resource::Channels | Resource::Guilds => BucketKey {
            method: method_enum,
            resource,
            major_id: major_id_str.to_owned(),
            sub_resource: classify_sub_resource(method, sub_path),
        },
        Resource::Webhooks => {
            let token = sub_path.split('/').next().filter(|s| s.len() >= 60);
            BucketKey {
                method: method_enum,
                resource,
                major_id: match token {
                    Some(t) => format!("{major_id_str}:{:016x}", fnv1a(t)),
                    None => major_id_str.to_owned(),
                },
                sub_resource: None,
            }
        }
        _ => BucketKey {
            method: method_enum,
            resource,
            major_id: String::from("!"),
            sub_resource: None,
        },
    }
}

#[inline]
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

#[inline]
fn strip_api_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("api/") {
        // Only strip the version segment if it starts with v{digit}
        if let Some(pos) = rest.find('/') {
            let segment = &rest[..pos];
            if segment.starts_with('v') && segment.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
            {
                return &rest[pos + 1..];
            }
        }
        // No version segment — strip just the "api/" prefix
        return rest;
    }
    path
}

#[inline]
fn classify_sub_resource(method: &str, sub_path: &str) -> Option<SubResource> {
    if sub_path.is_empty() {
        return None;
    }

    let first = match sub_path.split_once('/') {
        Some((f, _)) => f,
        None => sub_path,
    };

    match first {
        "messages" => {
            // Check for reactions: messages/{id}/reactions/...
            if let Some((_, after_first)) = sub_path.split_once('/') {
                if let Some((_, after_id)) = after_first.split_once('/') {
                    if after_id.starts_with("reactions") {
                        return if method == "PUT" || method == "DELETE" {
                            Some(SubResource::ReactionsModify)
                        } else {
                            Some(SubResource::Reactions)
                        };
                    }
                }
            }
            Some(SubResource::Messages)
        }
        "pins" => Some(SubResource::Pins),
        "members" => Some(SubResource::Members),
        "bans" => Some(SubResource::Bans),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_route() {
        let key = parse_bucket_key("GET", "/api/v10/channels/123456/messages");
        assert_eq!(key.resource, Resource::Channels);
        assert_eq!(key.major_id, "123456");
        assert_eq!(key.sub_resource, Some(SubResource::Messages));
    }

    #[test]
    fn parse_guild_route() {
        let key = parse_bucket_key("GET", "/api/v10/guilds/789/members");
        assert_eq!(key.resource, Resource::Guilds);
        assert_eq!(key.major_id, "789");
        assert_eq!(key.sub_resource, Some(SubResource::Members));
    }

    #[test]
    fn parse_webhook_route_without_token() {
        let key = parse_bucket_key("GET", "/api/v10/webhooks/111");
        assert_eq!(key.resource, Resource::Webhooks);
        assert_eq!(key.major_id, "111");
    }

    #[test]
    fn webhook_token_scopes_the_bucket() {
        let token_a = "a".repeat(68);
        let token_b = "b".repeat(68);
        let key_a = parse_bucket_key("POST", &format!("/api/v10/webhooks/111/{token_a}"));
        let key_b = parse_bucket_key("POST", &format!("/api/v10/webhooks/111/{token_b}"));
        let key_a2 = parse_bucket_key(
            "POST",
            &format!("/api/v10/webhooks/111/{token_a}/messages/@original"),
        );

        assert!(key_a.major_id.starts_with("111:"));
        assert!(!key_a.major_id.contains(&token_a));
        assert_ne!(key_a.major_id, key_b.major_id);
        assert_eq!(key_a.major_id, key_a2.major_id);
    }

    #[test]
    fn webhook_short_segment_is_not_a_token() {
        let key = parse_bucket_key("GET", "/api/v10/webhooks/111/github");
        assert_eq!(key.major_id, "111");
    }

    #[test]
    fn parse_reaction_modify() {
        let key = parse_bucket_key(
            "PUT",
            "/api/v10/channels/123/messages/456/reactions/\u{1f525}/@me",
        );
        assert_eq!(key.sub_resource, Some(SubResource::ReactionsModify));
    }

    #[test]
    fn parse_reaction_query() {
        let key = parse_bucket_key(
            "GET",
            "/api/v10/channels/123/messages/456/reactions/\u{1f525}",
        );
        assert_eq!(key.sub_resource, Some(SubResource::Reactions));
    }

    #[test]
    fn parse_invites() {
        let key = parse_bucket_key("GET", "/api/v10/invites/abc123");
        assert_eq!(key.resource, Resource::Invites);
        assert_eq!(key.major_id, "!");
    }

    #[test]
    fn handles_no_api_prefix() {
        let key = parse_bucket_key("GET", "/channels/123/messages");
        assert_eq!(key.resource, Resource::Channels);
        assert_eq!(key.major_id, "123");
    }

    #[test]
    fn handles_api_without_version() {
        // /api/channels/123/messages should NOT strip "channels" as a version segment
        let key = parse_bucket_key("GET", "/api/channels/123/messages");
        assert_eq!(key.resource, Resource::Channels);
        assert_eq!(key.major_id, "123");
        assert_eq!(key.sub_resource, Some(SubResource::Messages));
    }

    #[test]
    fn display_format() {
        let key = BucketKey {
            method: Method::Get,
            resource: Resource::Channels,
            major_id: "123".into(),
            sub_resource: Some(SubResource::Messages),
        };
        assert_eq!(key.to_string(), "GET:channels/123/messages");
    }

    #[test]
    fn is_interaction_true() {
        let key = parse_bucket_key("POST", "/api/v10/interactions/123/token/callback");
        assert!(key.is_interaction());
    }

    #[test]
    fn is_interaction_false() {
        let key = parse_bucket_key("GET", "/api/v10/channels/123/messages");
        assert!(!key.is_interaction());
    }

    #[test]
    fn method_stored_correctly() {
        let get = parse_bucket_key("GET", "/api/v10/channels/123/messages");
        let post = parse_bucket_key("POST", "/api/v10/channels/123/messages");
        assert_eq!(get.method, Method::Get);
        assert_eq!(post.method, Method::Post);
        assert_ne!(get, post);
    }
}

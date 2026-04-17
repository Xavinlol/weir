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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    pub resource: Resource,
    pub major_id: String,
    pub sub_resource: Option<SubResource>,
}

impl fmt::Display for BucketKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.sub_resource {
            Some(sub) => write!(f, "{}/{}/{sub}", self.resource, self.major_id),
            None => write!(f, "{}/{}", self.resource, self.major_id),
        }
    }
}

/// Parse a Discord API path into a bucket key for rate limiting.
#[inline]
pub fn parse_bucket_key(method: &str, path: &str) -> BucketKey {
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
            resource,
            major_id: major_id_str.to_owned(),
            sub_resource: classify_sub_resource(method, sub_path),
        },
        Resource::Webhooks => BucketKey {
            resource,
            major_id: major_id_str.to_owned(),
            sub_resource: None,
        },
        _ => BucketKey {
            resource,
            major_id: String::from("!"),
            sub_resource: None,
        },
    }
}

#[inline]
fn strip_api_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("api/") {
        if let Some(pos) = rest.find('/') {
            return &rest[pos + 1..];
        }
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
    fn parse_webhook_route() {
        let key = parse_bucket_key("POST", "/api/v10/webhooks/111/token");
        assert_eq!(key.resource, Resource::Webhooks);
        assert_eq!(key.major_id, "111");
    }

    #[test]
    fn parse_reaction_modify() {
        let key =
            parse_bucket_key("PUT", "/api/v10/channels/123/messages/456/reactions/\u{1f525}/@me");
        assert_eq!(key.sub_resource, Some(SubResource::ReactionsModify));
    }

    #[test]
    fn parse_reaction_query() {
        let key =
            parse_bucket_key("GET", "/api/v10/channels/123/messages/456/reactions/\u{1f525}");
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
    fn display_format() {
        let key = BucketKey {
            resource: Resource::Channels,
            major_id: "123".into(),
            sub_resource: Some(SubResource::Messages),
        };
        assert_eq!(key.to_string(), "channels/123/messages");
    }
}

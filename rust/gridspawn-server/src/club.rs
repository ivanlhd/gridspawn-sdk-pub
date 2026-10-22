//! Club posts (protocol 05): the club grant and the post.

use std::time::{Duration, Instant};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use url::Url;
use zeroize::Zeroizing;

use crate::{
    client::Client,
    error::{Error, ErrorCode, Service},
    http::{Auth, Call},
    types::{bounded, IdempotencyKey},
};

/// A grant is handed out again while more than this remains (the edge's rule).
const REUSE_MIN_REMAINING: Duration = Duration::from_secs(60);

pub(crate) struct CachedGrant {
    token: Zeroizing<String>,
    expires: Instant,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantResponse {
    access_token: String,
    token_type: String,
    expires_in: f64,
}

/// The body of a club post after `feature` (protocol 05 §2).
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Post {
    /// Board title, at most 160 characters. Without it the first line of
    /// `content` becomes the title.
    pub title: Option<String>,
    /// Board body (at most 8000 characters) or chat message (at most 2000).
    pub content: Option<String>,
    /// At most 400 characters.
    pub summary: Option<String>,
    /// http(s); becomes the board post's button.
    pub link: Option<Link>,
}

impl Post {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn titled(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    pub fn link(mut self, url: Url, label: impl Into<String>) -> Self {
        self.link = Some(Link {
            url,
            label: label.into(),
        });
        self
    }

    fn to_json(&self, feature: &str) -> Result<Value, Error> {
        let present =
            |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
        if !present(&self.title) && !present(&self.content) {
            return Err(Error::InvalidInput {
                field: "post",
                reason: "needs a title or content",
            });
        }
        let mut body = Map::new();
        body.insert("feature".into(), json!(feature));
        if let Some(title) = &self.title {
            bounded(title, 1, 160, "title")?;
            body.insert("title".into(), json!(title));
        }
        if let Some(content) = &self.content {
            if content.chars().count() > 8000 {
                return Err(Error::InvalidInput {
                    field: "content",
                    reason: "at most 8000 characters",
                });
            }
            body.insert("content".into(), json!(content));
        }
        if let Some(summary) = &self.summary {
            bounded(summary, 1, 400, "summary")?;
            body.insert("summary".into(), json!(summary));
        }
        if let Some(link) = &self.link {
            if !matches!(link.url.scheme(), "http" | "https") {
                return Err(Error::InvalidInput {
                    field: "link",
                    reason: "must be http or https",
                });
            }
            body.insert(
                "link".into(),
                json!({ "url": link.url.as_str(), "label": link.label }),
            );
        }
        Ok(Value::Object(body))
    }
}

#[derive(Clone, Debug)]
pub struct Link {
    pub url: Url,
    pub label: String,
}

/// What Core answered.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostReceipt {
    pub id: String,
    /// True when this Idempotency-Key had already posted; nothing new was posted.
    pub duplicate: bool,
}

#[derive(Deserialize)]
struct PostResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    duplicate: bool,
}

/// `^[a-z0-9][a-z0-9_.-]{0,47}$`, as in the Dev Hub.
fn valid_feature(feature: &str) -> bool {
    let bytes = feature.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

impl Client {
    /// The club grant: the cached one while more than 60 s remain, else a new one.
    async fn club_grant(&self, refresh: bool) -> Result<Zeroizing<String>, Error> {
        let mut cached = self.inner.grant.lock().await;
        if !refresh {
            if let Some(grant) = cached.as_ref() {
                if grant.expires.saturating_duration_since(Instant::now()) > REUSE_MIN_REMAINING {
                    return Ok(grant.token.clone());
                }
            }
        }
        let response: GrantResponse = self
            .edge_post(
                "POST /v1/server/grants/club",
                "/v1/server/grants/club",
                &json!({}),
                None,
                true,
            )
            .await?;
        let token = Zeroizing::new(response.access_token);
        if token.is_empty()
            || response.token_type != "Bearer"
            || !response.expires_in.is_finite()
            || response.expires_in <= 0.0
        {
            return Err(Error::InvalidResponse("invalid club grant"));
        }
        *cached = Some(CachedGrant {
            token: token.clone(),
            expires: Instant::now() + Duration::from_secs_f64(response.expires_in),
        });
        Ok(token)
    }

    /// Posts to the board or chat the game's club chose for `feature`
    /// (protocol 05). Reusing `idempotency_key` never posts twice. On
    /// `grant_invalid` the grant is renewed once and the post retried.
    pub async fn post(
        &self,
        feature: &str,
        body: Post,
        idempotency_key: &IdempotencyKey,
    ) -> Result<PostReceipt, Error> {
        if !valid_feature(feature) {
            return Err(Error::InvalidInput {
                field: "feature",
                reason: "1-48 characters of a-z 0-9 _ . - starting with a letter or digit",
            });
        }
        let payload = body.to_json(feature)?;
        let mut refreshed = false;
        loop {
            let grant = self.club_grant(refreshed).await?;
            let result: Result<PostResponse, Error> = self
                .inner
                .http
                .send(Call {
                    service: Service::Core,
                    method: Method::POST,
                    url: self.inner.club_posts.clone(),
                    route: "POST /core/v1/game-servers/posts",
                    auth: Auth::Bearer(grant.as_str()),
                    body: Some(&payload),
                    idempotency_key: Some(idempotency_key.as_str()),
                    retry: true,
                })
                .await;
            match result {
                Err(Error::Api(error)) if error.code == ErrorCode::GrantInvalid && !refreshed => {
                    tracing::debug!("club grant refused; getting a new one");
                    refreshed = true;
                }
                Err(error) => return Err(error),
                Ok(response) if response.id.is_empty() => {
                    return Err(Error::InvalidResponse("the post answer has no id"));
                }
                Ok(response) => {
                    return Ok(PostReceipt {
                        id: response.id,
                        duplicate: response.duplicate,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_keys_follow_the_dev_hub_rule() {
        for ok in ["patch-notes", "match.results", "a", "0day", "x_y"] {
            assert!(valid_feature(ok), "{ok}");
        }
        for bad in ["", "Patch", "-x", "a b", "über", &"a".repeat(49)] {
            assert!(!valid_feature(bad), "{bad}");
        }
    }

    #[test]
    fn post_bodies_carry_the_feature() {
        let body = Post::titled("Patch", "Notes")
            .summary("short")
            .link(Url::parse("https://example.com/p").unwrap(), "Read")
            .to_json("patch-notes")
            .unwrap();
        assert_eq!(
            body,
            json!({"feature":"patch-notes","title":"Patch","content":"Notes","summary":"short",
                   "link":{"url":"https://example.com/p","label":"Read"}})
        );
        assert!(Post::default().to_json("x").is_err());
        assert!(Post::text("x")
            .link(Url::parse("ftp://example.com").unwrap(), "x")
            .to_json("x")
            .is_err());
    }
}

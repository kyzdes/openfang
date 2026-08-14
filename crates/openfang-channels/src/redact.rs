//! Shared credential-redaction helpers for channel adapters.
//!
//! Several channel APIs authenticate by embedding a token or secret directly
//! in the request URL rather than in a header — Telegram's Bot API
//! (`/bot{token}/...`, no header alternative), and, via ad hoc query
//! parameters, DingTalk, Flock, Gotify, Messenger, Threema and WeCom.
//! `reqwest` attaches the request URL to connection-level errors (failed
//! connect, timeout, TLS, redirect, malformed-URI) so they're reproducible
//! from logs, but that means a bare `{e}` on such an error prints the
//! credential in cleartext wherever the error ends up — server logs, a
//! channel's own error reply, an admin dashboard. Call
//! [`redact_reqwest_error`] on every `reqwest::Error` coming out of
//! `.send()` before it is displayed or propagated, so the credential never
//! reaches a log line. This does not cover decode errors from
//! `.json()`/`.text()` — `reqwest` never attaches a URL to those in the
//! first place.

/// Strip the request URL from a `reqwest::Error` before it is logged or
/// otherwise displayed.
pub fn redact_reqwest_error(e: reqwest::Error) -> reqwest::Error {
    e.without_url()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> reqwest::Client {
        reqwest::Client::new()
    }

    /// FANG-39/FANG-44: `redact_reqwest_error` must strip a URL-embedded
    /// credential out of a `reqwest::Error`'s `Display` output. Connection-
    /// level errors carry the full request URL; without redaction, logging
    /// the error verbatim leaks whatever token/secret was in it (Telegram's
    /// bot token in the path, or the query-string tokens used by DingTalk,
    /// Flock, Gotify, Messenger, Threema and WeCom).
    ///
    /// Uses an invalid hostname to force a `Kind::Builder` error with a URL
    /// attached — deterministic and requires no real network I/O (mirrors
    /// reqwest's own `execute_request_rejects_invalid_hostname` test).
    #[tokio::test]
    async fn test_redact_reqwest_error_strips_token_from_url() {
        let token = "123456789:AAFakeTokenForTestingOnly-doNotUse";
        let bad_url = format!("https://{{{{hostname}}}}/bot{token}/getUpdates");

        let err = test_client()
            .get(&bad_url)
            .send()
            .await
            .expect_err("malformed hostname must fail before any network I/O");

        // Sanity check: confirm the token really is present pre-redaction,
        // otherwise this test would pass for the wrong reason.
        assert!(
            err.url().is_some(),
            "test assumption broken: reqwest stopped attaching url() to builder errors"
        );
        assert!(format!("{err}").contains(token));

        let redacted = redact_reqwest_error(err);
        let rendered = format!("{redacted}");
        assert!(
            !rendered.contains(token),
            "redacted error still leaks the credential: {rendered}"
        );
        assert!(redacted.url().is_none());
    }

    /// Same shape, but with a query-string-style credential (`?token=...`),
    /// matching how Gotify/Flock/Messenger/Threema/WeCom/DingTalk embed
    /// theirs — as opposed to Telegram's path-segment style above.
    #[tokio::test]
    async fn test_redact_reqwest_error_strips_query_string_token() {
        let secret = "s3cr3t-query-param-do-not-leak";
        let bad_url = format!("https://{{{{hostname}}}}/send?token={secret}");

        let err = test_client()
            .get(&bad_url)
            .send()
            .await
            .expect_err("malformed hostname must fail before any network I/O");

        assert!(format!("{err}").contains(secret));

        let redacted = redact_reqwest_error(err);
        assert!(!format!("{redacted}").contains(secret));
        assert!(redacted.url().is_none());
    }
}

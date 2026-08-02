//! GitHub adapter — extracts template fields from GitHub webhook
//! payloads.
//!
//! Dispatches on the `X-GitHub-Event` header. Each event type
//! exposes its canonical fields so the operator's
//! `bus-hooks.yaml` templates can reference them by simple Tera
//! variable name without writing JSONPath expressions:
//!
//! | event          | fields                                            |
//! |---------------:|---------------------------------------------------|
//! | `push`         | `repo`, `branch`, `pusher`, `commit_count`        |
//! | `pull_request` | `repo`, `action`, `pr_number`, `pr_title`, `pr_user`, `pr_url` |
//! | `issues`       | `repo`, `action`, `issue_number`, `issue_title`, `issue_user`, `issue_url` |
//! | `release`      | `repo`, `action`, `tag`, `release_name`, `release_url` |
//!
//! Anything else passes through with `repo` set + `event` set so
//! a generic rule like `match.event: ping` still works.
//!
//! BUS-3.2 ships the `push` extractor + sample rule. BUS-3.3
//! lights up the remaining three event types.

use std::collections::BTreeMap;

use serde_json::Value;

use super::matcher::Adapter;

/// The GitHub adapter — stateless; one instance is held by the
/// listener and shared across requests.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitHubAdapter;

impl Adapter for GitHubAdapter {
    fn extract(
        &self,
        headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Option<(String, BTreeMap<String, String>)> {
        let event = headers.get("x-github-event")?.clone();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();

        // Every GitHub event carries a `repository.full_name`
        // ("owner/name"). Anchor on it so templates can always
        // reference `{{ repo }}`.
        if let Some(repo) = body
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
        {
            fields.insert("repo".to_string(), repo.to_string());
        }

        match event.as_str() {
            "push" => {
                if let Some(refs) = body.pointer("/ref").and_then(Value::as_str) {
                    let branch = refs.strip_prefix("refs/heads/").unwrap_or(refs).to_string();
                    fields.insert("branch".to_string(), branch);
                }
                if let Some(pusher) = body.pointer("/pusher/name").and_then(Value::as_str) {
                    fields.insert("pusher".to_string(), pusher.to_string());
                }
                if let Some(commits) = body.pointer("/commits").and_then(Value::as_array) {
                    fields.insert("commit_count".to_string(), commits.len().to_string());
                }
                if let Some(head) = body.pointer("/head_commit/message").and_then(Value::as_str) {
                    // Trim to the first line — GitHub commit
                    // messages can be multi-paragraph.
                    let first = head.lines().next().unwrap_or("").to_string();
                    fields.insert("head_message".to_string(), first);
                }
            }
            "pull_request" => {
                if let Some(action) = body.pointer("/action").and_then(Value::as_str) {
                    fields.insert("action".to_string(), action.to_string());
                }
                if let Some(num) = body.pointer("/pull_request/number").and_then(Value::as_u64) {
                    fields.insert("pr_number".to_string(), num.to_string());
                }
                if let Some(title) = body.pointer("/pull_request/title").and_then(Value::as_str) {
                    fields.insert("pr_title".to_string(), title.to_string());
                }
                if let Some(user) = body
                    .pointer("/pull_request/user/login")
                    .and_then(Value::as_str)
                {
                    fields.insert("pr_user".to_string(), user.to_string());
                }
                if let Some(url) = body
                    .pointer("/pull_request/html_url")
                    .and_then(Value::as_str)
                {
                    fields.insert("pr_url".to_string(), url.to_string());
                }
            }
            "issues" => {
                if let Some(action) = body.pointer("/action").and_then(Value::as_str) {
                    fields.insert("action".to_string(), action.to_string());
                }
                if let Some(num) = body.pointer("/issue/number").and_then(Value::as_u64) {
                    fields.insert("issue_number".to_string(), num.to_string());
                }
                if let Some(title) = body.pointer("/issue/title").and_then(Value::as_str) {
                    fields.insert("issue_title".to_string(), title.to_string());
                }
                if let Some(user) = body.pointer("/issue/user/login").and_then(Value::as_str) {
                    fields.insert("issue_user".to_string(), user.to_string());
                }
                if let Some(url) = body.pointer("/issue/html_url").and_then(Value::as_str) {
                    fields.insert("issue_url".to_string(), url.to_string());
                }
            }
            "release" => {
                if let Some(action) = body.pointer("/action").and_then(Value::as_str) {
                    fields.insert("action".to_string(), action.to_string());
                }
                if let Some(tag) = body.pointer("/release/tag_name").and_then(Value::as_str) {
                    fields.insert("tag".to_string(), tag.to_string());
                }
                if let Some(name) = body.pointer("/release/name").and_then(Value::as_str) {
                    // Tags often have no human-readable name; fall
                    // back to the tag itself.
                    fields.insert("release_name".to_string(), name.to_string());
                } else if let Some(tag) = body.pointer("/release/tag_name").and_then(Value::as_str)
                {
                    fields.insert("release_name".to_string(), tag.to_string());
                }
                if let Some(url) = body.pointer("/release/html_url").and_then(Value::as_str) {
                    fields.insert("release_url".to_string(), url.to_string());
                }
            }
            _ => {
                // Unknown event — still expose the event name as a
                // field so a `match.event: ping` rule can fire and
                // template against `{{ event }}`.
                fields.insert("event".to_string(), event.clone());
            }
        }

        Some((event, fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(event: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("x-github-event".to_string(), event.to_string())])
    }

    #[test]
    fn missing_header_returns_none() {
        let body = json!({"repository": {"full_name": "foo/bar"}});
        let out = GitHubAdapter.extract(&BTreeMap::new(), &body);
        assert!(out.is_none());
    }

    #[test]
    fn push_event_extracts_repo_branch_pusher_commits() {
        let body = json!({
            "ref": "refs/heads/main",
            "repository": { "full_name": "matthewmackes/MDE-X" },
            "pusher": { "name": "matt" },
            "commits": [
                {"id": "abc", "message": "first"},
                {"id": "def", "message": "second\n\nbody line"},
            ],
            "head_commit": { "message": "second\n\nbody line" },
        });
        let (event, fields) = GitHubAdapter.extract(&headers("push"), &body).unwrap();
        assert_eq!(event, "push");
        assert_eq!(
            fields.get("repo").map(String::as_str),
            Some("matthewmackes/MDE-X")
        );
        assert_eq!(fields.get("branch").map(String::as_str), Some("main"));
        assert_eq!(fields.get("pusher").map(String::as_str), Some("matt"));
        assert_eq!(fields.get("commit_count").map(String::as_str), Some("2"));
        assert_eq!(
            fields.get("head_message").map(String::as_str),
            Some("second")
        );
    }

    #[test]
    fn push_event_strips_refs_heads_prefix_only_for_branches() {
        // Branch ref
        let body = json!({
            "ref": "refs/heads/feature/foo",
            "repository": {"full_name": "x/y"},
            "pusher": {"name": "a"},
            "commits": [],
        });
        let (_, fields) = GitHubAdapter.extract(&headers("push"), &body).unwrap();
        assert_eq!(
            fields.get("branch").map(String::as_str),
            Some("feature/foo")
        );

        // Tag ref — keep as-is so templates can still reference it.
        let body = json!({
            "ref": "refs/tags/v1.0",
            "repository": {"full_name": "x/y"},
            "pusher": {"name": "a"},
            "commits": [],
        });
        let (_, fields) = GitHubAdapter.extract(&headers("push"), &body).unwrap();
        assert_eq!(
            fields.get("branch").map(String::as_str),
            Some("refs/tags/v1.0")
        );
    }

    #[test]
    fn pull_request_event_extracts_all_fields() {
        let body = json!({
            "action": "opened",
            "pull_request": {
                "number": 42,
                "title": "Add webhooks",
                "user": {"login": "octocat"},
                "html_url": "https://github.com/x/y/pull/42",
            },
            "repository": {"full_name": "x/y"},
        });
        let (event, fields) = GitHubAdapter
            .extract(&headers("pull_request"), &body)
            .unwrap();
        assert_eq!(event, "pull_request");
        assert_eq!(fields.get("repo").map(String::as_str), Some("x/y"));
        assert_eq!(fields.get("action").map(String::as_str), Some("opened"));
        assert_eq!(fields.get("pr_number").map(String::as_str), Some("42"));
        assert_eq!(
            fields.get("pr_title").map(String::as_str),
            Some("Add webhooks")
        );
        assert_eq!(fields.get("pr_user").map(String::as_str), Some("octocat"));
        assert_eq!(
            fields.get("pr_url").map(String::as_str),
            Some("https://github.com/x/y/pull/42")
        );
    }

    #[test]
    fn issues_event_extracts_all_fields() {
        let body = json!({
            "action": "closed",
            "issue": {
                "number": 7,
                "title": "Crash on launch",
                "user": {"login": "alice"},
                "html_url": "https://github.com/x/y/issues/7",
            },
            "repository": {"full_name": "x/y"},
        });
        let (event, fields) = GitHubAdapter.extract(&headers("issues"), &body).unwrap();
        assert_eq!(event, "issues");
        assert_eq!(fields.get("action").map(String::as_str), Some("closed"));
        assert_eq!(fields.get("issue_number").map(String::as_str), Some("7"));
        assert_eq!(
            fields.get("issue_title").map(String::as_str),
            Some("Crash on launch")
        );
        assert_eq!(fields.get("issue_user").map(String::as_str), Some("alice"));
    }

    #[test]
    fn release_event_extracts_all_fields() {
        let body = json!({
            "action": "published",
            "release": {
                "tag_name": "v2.0.0",
                "name": "Mackes 2.0",
                "html_url": "https://github.com/x/y/releases/tag/v2.0.0",
            },
            "repository": {"full_name": "x/y"},
        });
        let (event, fields) = GitHubAdapter.extract(&headers("release"), &body).unwrap();
        assert_eq!(event, "release");
        assert_eq!(fields.get("tag").map(String::as_str), Some("v2.0.0"));
        assert_eq!(
            fields.get("release_name").map(String::as_str),
            Some("Mackes 2.0")
        );
    }

    #[test]
    fn release_falls_back_to_tag_when_name_absent() {
        let body = json!({
            "action": "published",
            "release": { "tag_name": "v2.0.0", "html_url": "x" },
            "repository": {"full_name": "x/y"},
        });
        let (_, fields) = GitHubAdapter.extract(&headers("release"), &body).unwrap();
        assert_eq!(
            fields.get("release_name").map(String::as_str),
            Some("v2.0.0")
        );
    }

    #[test]
    fn unknown_event_exposes_event_field_for_match_event_rules() {
        let body = json!({"repository": {"full_name": "x/y"}});
        let (event, fields) = GitHubAdapter.extract(&headers("ping"), &body).unwrap();
        assert_eq!(event, "ping");
        assert_eq!(fields.get("event").map(String::as_str), Some("ping"));
        assert_eq!(fields.get("repo").map(String::as_str), Some("x/y"));
    }
}

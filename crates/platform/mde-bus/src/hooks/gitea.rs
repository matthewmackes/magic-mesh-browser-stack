//! Gitea adapter — extracts template fields from Gitea webhook
//! payloads (BUS-3.4).
//!
//! Dispatches on the `X-Gitea-Event` header. Each event type
//! exposes its canonical fields so operator `bus-hooks.yaml`
//! templates can reference them by simple Tera variable name:
//!
//! | event          | fields                                            |
//! |---------------:|---------------------------------------------------|
//! | `push`         | `repo`, `branch`, `pusher`, `commit_count`        |
//! | `pull_request` | `repo`, `action`, `pr_number`, `pr_title`, `pr_user`, `pr_url` |
//! | `issues`       | `repo`, `action`, `issue_number`, `issue_title`, `issue_user`, `issue_url` |
//!
//! Gitea's webhook payloads track GitHub's shape closely (Gitea
//! ships GitHub-compatible mode by default); the deltas are
//! per-field naming (`pusher.username` vs `pusher.name` etc.).
//! The extractor accepts either naming so a single rule set in
//! `bus-hooks.yaml` works against both server flavors.

use std::collections::BTreeMap;

use serde_json::Value;

use super::matcher::Adapter;

/// The Gitea adapter — stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct GiteaAdapter;

impl Adapter for GiteaAdapter {
    fn extract(
        &self,
        headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Option<(String, BTreeMap<String, String>)> {
        let event = headers.get("x-gitea-event")?.clone();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();

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
                // Gitea uses `pusher.username` (newer) or
                // `pusher.login` (older). Try both.
                let pusher = body
                    .pointer("/pusher/username")
                    .and_then(Value::as_str)
                    .or_else(|| body.pointer("/pusher/login").and_then(Value::as_str))
                    .or_else(|| body.pointer("/pusher/full_name").and_then(Value::as_str));
                if let Some(p) = pusher {
                    fields.insert("pusher".to_string(), p.to_string());
                }
                if let Some(commits) = body.pointer("/commits").and_then(Value::as_array) {
                    fields.insert("commit_count".to_string(), commits.len().to_string());
                }
                if let Some(head) = body.pointer("/head_commit/message").and_then(Value::as_str) {
                    fields.insert(
                        "head_message".to_string(),
                        head.lines().next().unwrap_or("").to_string(),
                    );
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
                // Gitea uses `user.login` or `user.username`.
                let user = body
                    .pointer("/pull_request/user/login")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        body.pointer("/pull_request/user/username")
                            .and_then(Value::as_str)
                    });
                if let Some(u) = user {
                    fields.insert("pr_user".to_string(), u.to_string());
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
                let user = body
                    .pointer("/issue/user/login")
                    .and_then(Value::as_str)
                    .or_else(|| body.pointer("/issue/user/username").and_then(Value::as_str));
                if let Some(u) = user {
                    fields.insert("issue_user".to_string(), u.to_string());
                }
                if let Some(url) = body.pointer("/issue/html_url").and_then(Value::as_str) {
                    fields.insert("issue_url".to_string(), url.to_string());
                }
            }
            _ => {
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
        BTreeMap::from([("x-gitea-event".to_string(), event.to_string())])
    }

    #[test]
    fn push_extracts_with_username_field() {
        let body = json!({
            "ref": "refs/heads/main",
            "repository": {"full_name": "team/proj"},
            "pusher": {"username": "alice"},
            "commits": [{"id": "a"}, {"id": "b"}, {"id": "c"}],
            "head_commit": {"message": "fix the thing"},
        });
        let (event, fields) = GiteaAdapter.extract(&headers("push"), &body).unwrap();
        assert_eq!(event, "push");
        assert_eq!(fields.get("repo").map(String::as_str), Some("team/proj"));
        assert_eq!(fields.get("branch").map(String::as_str), Some("main"));
        assert_eq!(fields.get("pusher").map(String::as_str), Some("alice"));
        assert_eq!(fields.get("commit_count").map(String::as_str), Some("3"));
        assert_eq!(
            fields.get("head_message").map(String::as_str),
            Some("fix the thing")
        );
    }

    #[test]
    fn push_falls_back_to_login_then_full_name() {
        let body = json!({
            "ref": "refs/heads/main",
            "repository": {"full_name": "team/proj"},
            "pusher": {"login": "bob"},
            "commits": [],
        });
        let (_, fields) = GiteaAdapter.extract(&headers("push"), &body).unwrap();
        assert_eq!(fields.get("pusher").map(String::as_str), Some("bob"));

        let body = json!({
            "ref": "refs/heads/main",
            "repository": {"full_name": "team/proj"},
            "pusher": {"full_name": "Carol Operator"},
            "commits": [],
        });
        let (_, fields) = GiteaAdapter.extract(&headers("push"), &body).unwrap();
        assert_eq!(
            fields.get("pusher").map(String::as_str),
            Some("Carol Operator")
        );
    }

    #[test]
    fn pull_request_extracts_all_fields() {
        let body = json!({
            "action": "opened",
            "pull_request": {
                "number": 17,
                "title": "Add Gitea support",
                "user": {"username": "dev"},
                "html_url": "https://gitea.local/team/proj/pulls/17",
            },
            "repository": {"full_name": "team/proj"},
        });
        let (event, fields) = GiteaAdapter
            .extract(&headers("pull_request"), &body)
            .unwrap();
        assert_eq!(event, "pull_request");
        assert_eq!(fields.get("pr_user").map(String::as_str), Some("dev"));
        assert_eq!(fields.get("pr_number").map(String::as_str), Some("17"));
        assert_eq!(
            fields.get("pr_title").map(String::as_str),
            Some("Add Gitea support")
        );
        assert_eq!(
            fields.get("pr_url").map(String::as_str),
            Some("https://gitea.local/team/proj/pulls/17")
        );
    }

    #[test]
    fn issues_extracts_all_fields() {
        let body = json!({
            "action": "closed",
            "issue": {
                "number": 5,
                "title": "Disk full on backup",
                "user": {"login": "ops"},
                "html_url": "https://gitea.local/team/proj/issues/5",
            },
            "repository": {"full_name": "team/proj"},
        });
        let (event, fields) = GiteaAdapter.extract(&headers("issues"), &body).unwrap();
        assert_eq!(event, "issues");
        assert_eq!(fields.get("issue_user").map(String::as_str), Some("ops"));
    }

    #[test]
    fn missing_header_returns_none() {
        let body = json!({});
        assert!(GiteaAdapter.extract(&BTreeMap::new(), &body).is_none());
    }
}

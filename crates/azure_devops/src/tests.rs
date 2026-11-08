use super::*;
use std::{cell::RefCell, collections::VecDeque};

#[derive(Default)]
struct Fake {
    responses: RefCell<VecDeque<String>>,
    calls: RefCell<Vec<Vec<String>>>,
}
impl Fake {
    fn with(responses: Vec<Value>) -> Self {
        Self {
            responses: RefCell::new(responses.into_iter().map(|v| v.to_string()).collect()),
            ..Self::default()
        }
    }
}
impl Runner for Fake {
    fn az(&self, args: &[String], _: Duration) -> Result<String> {
        self.calls.borrow_mut().push(args.to_vec());
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| "Unexpected request".into())
    }
}
fn connection() -> Connection {
    Connection {
        organization: "contoso".into(),
        project: "My Project".into(),
        repository: "repo".into(),
        tenant: String::new(),
    }
}
fn arg<'a>(args: &'a [String], key: &str) -> &'a str {
    &args[args.iter().position(|s| s == key).unwrap() + 1]
}

#[test]
fn detects_microsoft_remote_formats_and_decodes_names() {
    for remote in [
        "https://user@dev.azure.com/contoso/My%20Project/_git/repo",
        "git@ssh.dev.azure.com:v3/contoso/My%20Project/repo",
        "ssh://git@ssh.dev.azure.com/v3/contoso/My%20Project/repo",
        "https://contoso.visualstudio.com/My%20Project/_git/repo",
        "https://contoso.visualstudio.com/DefaultCollection/My%20Project/_git/repo",
    ] {
        assert_eq!(
            Connection::from_remote(remote),
            Some(connection()),
            "{remote}"
        );
    }
    for remote in [
        "https://dev.azure.com.evil.test/contoso/project/_git/repo",
        "https://evil.test/contoso/project/_git/repo",
        "http://dev.azure.com/contoso/project/_git/repo",
        "git@ssh.dev.azure.com:v3/org/project",
        "https://dev.azure.com/org/project/_git/repo/extra",
    ] {
        assert!(Connection::from_remote(remote).is_none(), "{remote}");
    }
}

#[test]
fn urls_encode_components_and_keep_the_trusted_host() {
    let mut c = connection();
    c.repository = "repo?#&x=1".into();
    let url = c
        .api(&["git", "repositories", &c.repository, "pullrequests"])
        .unwrap();
    assert_eq!(url.host_str(), Some("dev.azure.com"));
    assert!(url.path().contains("My%20Project"));
    assert!(url.path().contains("repo%3F%23&x=1"));
    assert_eq!(url.query(), Some("api-version=7.1"));
    c.organization = "evil.test/path".into();
    assert!(c.api(&[]).is_err());
}

#[test]
fn open_prs_paginate_and_filter_active() {
    let page: Vec<_> = (0..100)
        .map(|i| json!({"pullRequestId": i, "title": "PR"}))
        .collect();
    let client = Client::new(
        connection(),
        Fake::with(vec![
            json!({"value":page}),
            json!({"value":[{"pullRequestId":100,"title":"Last"}]}),
        ]),
    );
    assert_eq!(client.pull_requests().unwrap().len(), 101);
    let calls = client.runner.calls.borrow();
    assert_eq!(calls.len(), 2);
    for (i, call) in calls.iter().enumerate() {
        let url = Url::parse(arg(call, "--url")).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["searchCriteria.status"], "active");
        assert_eq!(query["$skip"], (i * 100).to_string());
        assert_eq!(arg(call, "--resource"), RESOURCE);
    }
}

#[test]
fn empty_repository_lists_project_prs() {
    let mut c = connection();
    c.repository.clear();
    let client = Client::new(c, Fake::with(vec![json!({"value":[]})]));
    client.pull_requests().unwrap();
    assert!(arg(&client.runner.calls.borrow()[0], "--url").contains("/_apis/git/pullrequests?"));
}

#[test]
fn queue_run_preserves_parameters_and_normalizes_branch() {
    let client = Client::new(
        connection(),
        Fake::with(vec![json!({"id":42,"name":"run-42"})]),
    );
    let build = client
        .run(7, "feature/fix", r#"{"test":true,"name":"a b"}"#)
        .unwrap();
    assert_eq!(build.id, 42);
    assert!(build.active());
    let calls = client.runner.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(arg(&calls[0], "--method"), "post");
    let body: Value = serde_json::from_str(arg(&calls[0], "--body")).unwrap();
    assert_eq!(
        body["resources"]["repositories"]["self"]["refName"],
        "refs/heads/feature/fix"
    );
    assert_eq!(body["templateParameters"]["test"], true);
}

#[test]
fn rejects_bad_run_inputs_without_network() {
    let client = Client::new(connection(), Fake::default());
    for (branch, params) in [
        ("", "{}"),
        ("main", "[]"),
        ("main", "null"),
        ("main", "bad json"),
    ] {
        assert!(client.run(7, branch, params).is_err());
    }
    assert!(client.runner.calls.borrow().is_empty());
}

#[test]
fn sso_targets_devops_and_optional_tenant_without_secrets() {
    let mut c = connection();
    c.tenant = "example.onmicrosoft.com".into();
    let client = Client::new(c, Fake::with(vec![Value::Null]));
    client.sign_in().unwrap();
    let calls = client.runner.calls.borrow();
    assert_eq!(calls[0][0], "login");
    assert_eq!(arg(&calls[0], "--scope"), format!("{RESOURCE}/.default"));
    assert_eq!(arg(&calls[0], "--tenant"), "example.onmicrosoft.com");
    assert_eq!(arg(&calls[0], "--output"), "none");
}

#[test]
fn timeline_orders_children_and_retains_orphans_and_cycles() {
    let record = |id: &str, parent: Option<&str>, order| Record {
        id: id.into(),
        parent_id: parent.map(String::from),
        order,
        ..Record::default()
    };
    let records = vec![
        record("step", Some("job"), 2),
        record("job", Some("stage"), 1),
        record("stage", None, 0),
        record("orphan", Some("missing"), 3),
        record("cycle-a", Some("cycle-b"), 4),
        record("cycle-b", Some("cycle-a"), 5),
    ];
    let rows = timeline_rows(&records);
    assert_eq!(&rows[..4], &[(2, 0), (1, 1), (0, 2), (3, 0)]);
    let mut indices: Vec<_> = rows.iter().map(|r| r.0).collect();
    indices.sort();
    assert_eq!(indices, (0..6).collect::<Vec<_>>());
}

#[test]
fn queued_builds_can_have_null_results() {
    let client = Client::new(
        connection(),
        Fake::with(vec![json!({"id":42,
        "status":"inProgress", "result":null, "buildNumber":"42"})]),
    );
    let build = client.build(42).unwrap();
    assert_eq!(build.label(), "inProgress");
    assert!(build.active());
}

#[test]
fn failed_queue_requests_are_never_retried() {
    let client = Client::new(connection(), Fake::default());
    assert!(client.run(7, "refs/heads/main", "{}").is_err());
    assert_eq!(client.runner.calls.borrow().len(), 1);
}

#[test]
fn browser_links_encode_repo_and_build_ids() {
    let c = connection();
    assert_eq!(
        c.web_url("pr", 42).unwrap(),
        "https://dev.azure.com/contoso/My%20Project/_git/repo/pullrequest/42"
    );
    assert_eq!(
        c.web_url("build", 77).unwrap(),
        "https://dev.azure.com/contoso/My%20Project/_build/results?buildId=77"
    );
}

#[test]
fn iso_times_parse_with_fractions_and_offsets() {
    assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0.));
    assert_eq!(unix_seconds("2026-10-28T20:40:12Z"), Some(1_793_220_012.));
    let fractional = unix_seconds("2026-10-28T20:40:12.5000000Z").unwrap();
    assert!((fractional - 1_793_220_012.5).abs() < 1e-6);
    assert_eq!(
        unix_seconds("2026-10-28T22:40:12+02:00"),
        Some(1_793_220_012.)
    );
    assert_eq!(unix_seconds("not a time"), None);
    let build: Build = serde_json::from_value(
        json!({"id": 1, "status": "completed", "result": "succeeded",
        "startTime": "2026-10-28T20:40:00Z", "finishTime": "2026-10-28T20:45:30Z",
        "requestedFor": {"displayName": "Ada"}}),
    )
    .unwrap();
    assert_eq!(build.duration(0.), Some(330.));
    assert_eq!(build.requested_for.display_name, "Ada");
    // Still running: up to now.
    assert_eq!(span(Some("1970-01-01T00:00:10Z"), None, 70.), Some(60.));
}

#[test]
fn pr_details_read_policies_threads_and_votes() {
    let pr: PullRequest = serde_json::from_value(json!({"pullRequestId": 42,
        "repository": {"id": "repo-guid", "name": "repo", "project": {"id": "project-guid"}},
        "reviewers": [{"displayName": "Ada", "vote": 10}, {"displayName": "Grace", "vote": 5},
            {"displayName": "[Team]", "vote": 10, "isContainer": true}, {"displayName": "Linus", "vote": -5}]}))
    .unwrap();
    assert_eq!(pr.approvals(), 2);
    assert!(pr.waiting_for_author() && !pr.rejected());
    let policy = |id: &str, status: &str, settings: Value| {
        json!({"status": status, "configuration": {"isEnabled": true, "isBlocking": true,
            "type": {"id": id, "displayName": "Build"}, "settings": settings}})
    };
    let fake = Fake::with(vec![
        json!({"value": [
            policy(MINIMUM_REVIEWERS, "rejected", json!({"minimumApproverCount": 3})),
            policy(COMMENT_REQUIREMENTS, "rejected", json!({})),
            policy("0609b952-1397-4640-95ec-e00a01b2c241", "rejected", json!({"displayName": "web-ci"})),
            policy("0609b952-1397-4640-95ec-e00a01b2c241", "running", json!({})),
            policy("cbdc66da-9728-4af8-aada-9a5a32e4a226", "approved", json!({"statusName": "sonar"})),
            policy("0609b952-1397-4640-95ec-e00a01b2c241", "notApplicable", json!({"displayName": "skipped"})),
        ]}),
        json!({"value": [
            {"status": "active", "comments": [{"commentType": "text"}]},
            {"status": "fixed", "comments": [{"commentType": "text"}]},
            {"comments": [{"commentType": "text"}]},
            {"status": "active", "isDeleted": true, "comments": [{"commentType": "text"}]},
            {"comments": [{"commentType": "system"}]},
        ]}),
    ]);
    let client = Client::new(connection(), fake);
    let details = client.pr_details(&pr).unwrap();
    assert_eq!(details.approvals_needed, Some(3));
    assert_eq!(
        details.checks,
        vec![
            ("web-ci".into(), Check::Failed),
            ("Build".into(), Check::Pending),
            ("sonar".into(), Check::Passed)
        ]
    );
    assert_eq!((details.threads, details.active_threads), (3, 1));
    let calls = client.runner.calls.borrow();
    let policies = Url::parse(arg(&calls[0], "--url")).unwrap();
    assert_eq!(
        policies.path(),
        "/contoso/My%20Project/_apis/policy/evaluations"
    );
    let query: Vec<_> = policies
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        query,
        [
            (
                "artifactId".into(),
                "vstfs:///CodeReview/CodeReviewId/project-guid/42".into()
            ),
            ("api-version".into(), "7.1-preview.1".into())
        ]
    );
    assert!(arg(&calls[1], "--url")
        .starts_with("https://dev.azure.com/contoso/My%20Project/_apis/git/repositories/repo-guid/pullRequests/42/threads?"));
}

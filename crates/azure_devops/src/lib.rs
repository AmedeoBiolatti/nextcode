//! Azure DevOps Services through Azure CLI's Entra sign-in. No PATs, client
//! secrets or tokens cross this API. The host runs argument vectors locally;
//! Git discovery can happen independently on the project's remote machine.
use std::time::Duration;

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

pub const RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";
pub type Result<T> = std::result::Result<T, String>;

pub trait Runner {
    fn az(&self, args: &[String], timeout: Duration) -> Result<String>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Connection {
    pub organization: String,
    pub project: String,
    /// Repository name or ID; empty lists PRs across the project.
    pub repository: String,
    /// Optional Entra tenant ID or domain for sign-in.
    pub tenant: String,
}

impl Connection {
    pub fn validate(&self) -> Result<()> {
        if self.organization.is_empty()
            || !self
                .organization
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err("Enter the organization name (for example, contoso).".into());
        }
        if self.project.trim().is_empty() || matches!(self.project.as_str(), "." | "..") {
            return Err("Enter a project name.".into());
        }
        Ok(())
    }

    fn api(&self, segments: &[&str]) -> Result<Url> {
        self.validate()?;
        let mut url = Url::parse("https://dev.azure.com/").unwrap();
        {
            let mut path = url.path_segments_mut().unwrap();
            path.pop_if_empty()
                .push(&self.organization)
                .push(&self.project)
                .push("_apis");
            for segment in segments {
                if *segment == "." || *segment == ".." {
                    return Err("Invalid Azure DevOps path component.".into());
                }
                path.push(segment);
            }
        }
        url.query_pairs_mut().append_pair("api-version", "7.1");
        Ok(url)
    }

    pub fn web_url(&self, area: &str, id: i64) -> Result<String> {
        self.validate()?;
        let mut url = Url::parse("https://dev.azure.com/").unwrap();
        let mut path = url.path_segments_mut().unwrap();
        path.pop_if_empty()
            .push(&self.organization)
            .push(&self.project);
        match area {
            "pr" if !self.repository.is_empty() => {
                path.push("_git")
                    .push(&self.repository)
                    .push("pullrequest")
                    .push(&id.to_string());
            }
            "pr" => {
                path.push("_pulls");
            }
            _ => {
                path.push("_build").push("results");
            }
        }
        drop(path);
        if area != "pr" {
            url.query_pairs_mut()
                .append_pair("buildId", &id.to_string());
        }
        Ok(url.into())
    }

    /// HTTPS (including legacy visualstudio.com) and Azure SSH Git URLs.
    /// Only Microsoft's exact hostnames are accepted.
    pub fn from_remote(remote: &str) -> Option<Self> {
        let remote = remote.trim();
        if let Some(path) = remote.strip_prefix("git@ssh.dev.azure.com:v3/") {
            return Self::from_parts(path.split('/').collect());
        }
        let url = Url::parse(remote).ok()?;
        let host = url.host_str()?;
        let parts: Vec<_> = url.path_segments()?.collect();
        if url.scheme() == "https" && url.port().is_none() && host == "dev.azure.com" {
            if parts.len() == 4 && parts[2] == "_git" {
                return Self::from_parts(vec![parts[0], parts[1], parts[3]]);
            }
        } else if url.scheme() == "ssh" && host == "ssh.dev.azure.com" {
            if parts.first() == Some(&"v3") {
                return Self::from_parts(parts[1..].to_vec());
            }
        } else if url.scheme() == "https"
            && url.port().is_none()
            && let Some(org) = host.strip_suffix(".visualstudio.com")
        {
            let parts = parts.strip_prefix(&["DefaultCollection"]).unwrap_or(&parts);
            if parts.len() == 3 && parts[1] == "_git" {
                return Self::from_parts(vec![org, parts[0], parts[2]]);
            }
        }
        None
    }

    fn from_parts(parts: Vec<&str>) -> Option<Self> {
        if parts.len() != 3 {
            return None;
        }
        let decode = |s: &str| percent_decode_str(s).decode_utf8().ok().map(String::from);
        let connection = Self {
            organization: decode(parts[0])?,
            project: decode(parts[1])?,
            repository: decode(parts[2])?,
            tenant: String::new(),
        };
        connection.validate().ok()?;
        if connection.repository.is_empty() {
            return None;
        }
        Some(connection)
    }
}

pub struct Client<R> {
    pub connection: Connection,
    runner: R,
}

impl<R: Runner> Client<R> {
    pub fn new(connection: Connection, runner: R) -> Self {
        Self { connection, runner }
    }

    pub fn sign_in(&self) -> Result<()> {
        let mut args = vec![
            "login".into(),
            "--allow-no-subscriptions".into(),
            "--scope".into(),
            format!("{RESOURCE}/.default"),
            "--output".into(),
            "none".into(),
            "--only-show-errors".into(),
        ];
        if !self.connection.tenant.trim().is_empty() {
            args.extend(["--tenant".into(), self.connection.tenant.trim().into()]);
        }
        self.runner.az(&args, Duration::from_secs(300)).map(|_| ())
    }

    fn request(&self, method: &str, url: Url, body: Option<Value>) -> Result<String> {
        let mut args = vec![
            "rest".into(),
            "--method".into(),
            method.into(),
            "--url".into(),
            url.into(),
            "--resource".into(),
            RESOURCE.into(),
            "--output".into(),
            "json".into(),
            "--only-show-errors".into(),
        ];
        if let Some(body) = body {
            args.extend([
                "--headers".into(),
                "Content-Type=application/json".into(),
                "--body".into(),
                body.to_string(),
            ]);
        }
        // Never retry a POST: a timed-out run may already have been queued.
        self.runner.az(&args, Duration::from_secs(90))
    }

    fn get<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        serde_json::from_str(&self.request("get", url, None)?)
            .map_err(|_| "Azure DevOps returned an unexpected response.".into())
    }

    pub fn pull_requests(&self) -> Result<Vec<PullRequest>> {
        let mut segments = vec!["git"];
        if !self.connection.repository.is_empty() {
            segments.extend(["repositories", &self.connection.repository]);
        }
        segments.push("pullrequests");
        let mut all = Vec::new();
        loop {
            let mut url = self.connection.api(&segments)?;
            url.query_pairs_mut()
                .append_pair("searchCriteria.status", "active")
                .append_pair("$top", "100")
                .append_pair("$skip", &all.len().to_string());
            let page: Collection<PullRequest> = self.get(url)?;
            let done = page.value.len() < 100;
            all.extend(page.value);
            if done {
                return Ok(all);
            }
            if all.len() >= 10000 {
                return Err("Too many open PRs; choose a repository to narrow the list.".into());
            }
        }
    }

    /// The checks, required approvals and comment threads of an open pull request: two requests.
    pub fn pr_details(&self, pr: &PullRequest) -> Result<PrDetails> {
        if pr.repository.id.is_empty() || pr.repository.project.id.is_empty() {
            return Err("The pull request did not name its repository.".into());
        }
        let mut url = self.connection.api(&["policy", "evaluations"])?;
        // A preview API: Azure DevOps refuses it without the suffix.
        url.set_query(None);
        url.query_pairs_mut()
            .append_pair(
                "artifactId",
                &format!(
                    "vstfs:///CodeReview/CodeReviewId/{}/{}",
                    pr.repository.project.id, pr.pull_request_id
                ),
            )
            .append_pair("api-version", "7.1-preview.1");
        let evaluations = self.get::<Collection<Evaluation>>(url)?.value;
        let url = self.connection.api(&[
            "git",
            "repositories",
            &pr.repository.id,
            "pullRequests",
            &pr.pull_request_id.to_string(),
            "threads",
        ])?;
        let threads = self.get::<Collection<Thread>>(url)?.value;
        Ok(details(evaluations, threads))
    }

    /// Recent build definitions. The UI labels the bound explicitly.
    pub fn pipelines(&self) -> Result<Vec<Pipeline>> {
        let mut url = self.connection.api(&["build", "definitions"])?;
        url.query_pairs_mut().append_pair("$top", "100");
        Ok(self.get::<Collection<Pipeline>>(url)?.value)
    }

    pub fn runs(&self, pipeline: i64) -> Result<Vec<Build>> {
        let mut url = self.connection.api(&["build", "builds"])?;
        url.query_pairs_mut()
            .append_pair("definitions", &pipeline.to_string())
            .append_pair("$top", "30")
            .append_pair("queryOrder", "queueTimeDescending");
        Ok(self.get::<Collection<Build>>(url)?.value)
    }

    pub fn run(&self, pipeline: i64, branch: &str, parameters: &str) -> Result<Build> {
        if branch.trim().is_empty() {
            return Err("Enter a branch to run.".into());
        }
        let parameters: Value = if parameters.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(parameters).map_err(|_| "Parameters must be a JSON object.")?
        };
        if !parameters.is_object() {
            return Err("Parameters must be a JSON object.".into());
        }
        let branch = branch.trim();
        let branch = if branch.starts_with("refs/heads/") {
            branch.into()
        } else {
            format!("refs/heads/{branch}")
        };
        let body = json!({"resources": {"repositories": {"self": {"refName": branch}}},
            "templateParameters": parameters});
        let url = self
            .connection
            .api(&["pipelines", &pipeline.to_string(), "runs"])?;
        let result: Value = serde_json::from_str(&self.request("post", url, Some(body))?)
            .map_err(|_| "Unexpected run response. Check Azure DevOps before running again.")?;
        let id = result["id"]
            .as_i64()
            .ok_or("Run response had no ID. Check Azure DevOps before running again.")?;
        Ok(Build {
            id,
            build_number: result["name"].as_str().unwrap_or("Queued run").into(),
            status: "notStarted".into(),
            result: String::new(),
            source_branch: branch,
            ..Build::default()
        })
    }

    pub fn build(&self, id: i64) -> Result<Build> {
        self.get(self.connection.api(&["build", "builds", &id.to_string()])?)
    }

    pub fn timeline(&self, build: i64) -> Result<Vec<Record>> {
        let value: Timeline =
            self.get(
                self.connection
                    .api(&["build", "builds", &build.to_string(), "timeline"])?,
            )?;
        Ok(value.records)
    }

    pub fn log(&self, build: i64, log: i64) -> Result<String> {
        let mut url = self.connection.api(&[
            "build",
            "builds",
            &build.to_string(),
            "logs",
            &log.to_string(),
        ])?;
        url.query_pairs_mut().append_pair("$format", "text");
        self.request("get", url, None)
    }
}

#[derive(Deserialize)]
struct Collection<T> {
    value: Vec<T>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Identity {
    pub display_name: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub project: ProjectRef,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectRef {
    pub id: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Reviewer {
    pub display_name: String,
    /// 10 approved, 5 approved with suggestions, 0 no vote, -5 waiting for the author, -10 rejected.
    pub vote: i32,
    pub is_required: bool,
    /// A group or team; its vote stands for one of its members'.
    pub is_container: bool,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PullRequest {
    pub pull_request_id: i64,
    pub title: String,
    pub created_by: Identity,
    pub source_ref_name: String,
    pub target_ref_name: String,
    pub is_draft: bool,
    pub repository: Repository,
    /// ISO 8601, as Azure DevOps writes it.
    pub creation_date: String,
    pub reviewers: Vec<Reviewer>,
}
impl PullRequest {
    /// People who approved (with or without suggestions); a group's vote repeats a member's.
    pub fn approvals(&self) -> usize {
        self.reviewers
            .iter()
            .filter(|r| !r.is_container && r.vote >= 5)
            .count()
    }
    pub fn rejected(&self) -> bool {
        self.reviewers.iter().any(|r| r.vote == -10)
    }
    pub fn waiting_for_author(&self) -> bool {
        self.reviewers.iter().any(|r| r.vote == -5)
    }
}

/// What a pull request's policies and threads say: the checks, the approvals it needs and its comments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrDetails {
    /// The approvals the minimum-reviewers policy asks for; `None` without one.
    pub approvals_needed: Option<u32>,
    /// Policies other than reviewers and comments (builds, status checks, work items…), as (name, outcome).
    pub checks: Vec<(String, Check)>,
    /// Comment threads people started (not system messages), and how many of them are unresolved.
    pub threads: usize,
    pub active_threads: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Passed,
    Pending,
    Failed,
}
impl PrDetails {
    pub fn count(&self, outcome: Check) -> usize {
        self.checks.iter().filter(|(_, c)| *c == outcome).count()
    }
}

const MINIMUM_REVIEWERS: &str = "fa4e907d-c16b-4a4c-9dfa-4906e5d171dd";
const REQUIRED_REVIEWERS: &str = "fd2167ab-b0be-447a-8ec8-39368250530e";
const COMMENT_REQUIREMENTS: &str = "c6a1889d-b943-4856-b76f-9e46bb6b0df2";

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Evaluation {
    status: String,
    configuration: PolicyConfiguration,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PolicyConfiguration {
    is_enabled: bool,
    is_blocking: bool,
    is_deleted: bool,
    #[serde(rename = "type")]
    kind: PolicyType,
    settings: Value,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PolicyType {
    id: String,
    display_name: String,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Thread {
    status: Option<String>,
    is_deleted: bool,
    comments: Vec<Comment>,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Comment {
    comment_type: String,
    is_deleted: bool,
}

fn details(evaluations: Vec<Evaluation>, threads: Vec<Thread>) -> PrDetails {
    let mut details = PrDetails::default();
    for evaluation in evaluations {
        let policy = &evaluation.configuration;
        if !policy.is_enabled || policy.is_deleted || evaluation.status == "notApplicable" {
            continue;
        }
        match policy.kind.id.as_str() {
            MINIMUM_REVIEWERS => {
                if policy.is_blocking
                    && let Some(count) = policy.settings["minimumApproverCount"].as_u64()
                {
                    let count = count.min(u32::MAX.into()) as u32;
                    details.approvals_needed =
                        Some(details.approvals_needed.unwrap_or(0).max(count));
                }
            }
            // The reviewers and comments columns cover these.
            REQUIRED_REVIEWERS | COMMENT_REQUIREMENTS => {}
            _ => {
                let name = ["displayName", "statusName"]
                    .iter()
                    .find_map(|key| policy.settings[key].as_str().filter(|s| !s.is_empty()))
                    .unwrap_or(&policy.kind.display_name);
                let outcome = match evaluation.status.as_str() {
                    "approved" => Check::Passed,
                    "rejected" | "broken" => Check::Failed,
                    _ => Check::Pending,
                };
                details.checks.push((name.to_owned(), outcome));
            }
        }
    }
    for thread in threads {
        let human = thread
            .comments
            .first()
            .is_some_and(|c| c.comment_type == "text" && !c.is_deleted);
        if thread.is_deleted || !human {
            continue;
        }
        details.threads += 1;
        // A thread without a status is a plain comment that needs no resolving.
        if matches!(thread.status.as_deref(), Some("active" | "pending")) {
            details.active_threads += 1;
        }
    }
    details
}
#[derive(Clone, Debug, Deserialize)]
pub struct Pipeline {
    pub id: i64,
    pub name: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Build {
    pub id: i64,
    pub build_number: String,
    pub status: String,
    #[serde(deserialize_with = "null_string")]
    pub result: String,
    pub source_branch: String,
    pub requested_for: Identity,
    /// ISO 8601 times; absent until the run reaches that point.
    pub queue_time: Option<String>,
    pub start_time: Option<String>,
    pub finish_time: Option<String>,
}
fn null_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}
impl Build {
    pub fn active(&self) -> bool {
        self.status != "completed"
    }
    pub fn label(&self) -> &str {
        if self.result.is_empty() {
            &self.status
        } else {
            &self.result
        }
    }
    /// Seconds from start to finish (to `now` while it runs); `None` before it starts.
    pub fn duration(&self, now: f64) -> Option<f64> {
        span(self.start_time.as_deref(), self.finish_time.as_deref(), now)
    }
}

/// Seconds between two ISO 8601 times, the end defaulting to `now` (Unix seconds).
pub fn span(start: Option<&str>, finish: Option<&str>, now: f64) -> Option<f64> {
    let start = unix_seconds(start?)?;
    let finish = finish.and_then(unix_seconds).unwrap_or(now);
    Some((finish - start).max(0.))
}

/// Unix seconds of an ISO 8601 / RFC 3339 time such as `2026-10-28T20:40:12.1234567Z` or with a `+02:00` offset.
pub fn unix_seconds(text: &str) -> Option<f64> {
    let text = text.trim();
    let (date, time) = text.split_once(['T', ' '])?;
    let mut date = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, offset) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else if let Some(at) = time.rfind(['+', '-']) {
        let (clock, zone) = time.split_at(at);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (h, m) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
        (
            clock,
            sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60),
        )
    } else {
        (time, 0)
    };
    let mut parts = clock.splitn(3, ':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: f64 = parts.next().unwrap_or("0").parse().ok()?;
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 - offset) as f64 + second)
}
#[derive(Deserialize)]
struct Timeline {
    records: Vec<Record>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Record {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub state: String,
    pub result: Option<String>,
    pub order: i64,
    pub percent_complete: Option<u32>,
    pub log: Option<Log>,
    pub start_time: Option<String>,
    pub finish_time: Option<String>,
    pub error_count: u32,
    pub warning_count: u32,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Log {
    pub id: i64,
}
impl Record {
    pub fn label(&self) -> &str {
        self.result.as_deref().unwrap_or(&self.state)
    }
}

/// Timeline records are flat and not guaranteed to be ordered. Build a
/// stable hierarchy, retaining orphans and breaking malformed parent cycles.
pub fn timeline_rows(records: &[Record]) -> Vec<(usize, usize)> {
    fn visit(
        index: usize,
        depth: usize,
        records: &[Record],
        seen: &mut [bool],
        rows: &mut Vec<(usize, usize)>,
    ) {
        if seen[index] {
            return;
        }
        seen[index] = true;
        rows.push((index, depth));
        let mut children: Vec<_> = records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.parent_id.as_deref() == Some(&records[index].id))
            .map(|(i, _)| i)
            .collect();
        children.sort_by_key(|&i| records[i].order);
        for child in children {
            visit(child, depth + 1, records, seen, rows);
        }
    }
    let mut indices: Vec<_> = (0..records.len()).collect();
    indices.sort_by_key(|&i| records[i].order);
    let mut seen = vec![false; records.len()];
    let mut rows = Vec::new();
    for &i in &indices {
        if records[i]
            .parent_id
            .as_ref()
            .is_none_or(|p| !records.iter().any(|r| &r.id == p))
        {
            visit(i, 0, records, &mut seen, &mut rows);
        }
    }
    for i in indices {
        visit(i, 0, records, &mut seen, &mut rows);
    }
    rows
}

#[cfg(test)]
mod tests;

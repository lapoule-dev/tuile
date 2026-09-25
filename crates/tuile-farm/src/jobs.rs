// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Managed batch jobs: start an execution, follow it to its end.
//!
//! A farm job is a *definition* owned by infrastructure code — image, card,
//! resources, timeout, the bucket key as a secret reference — and a launcher
//! asks for an *instance* of it: an execution, with the environment of one run
//! and a task count. That is the whole API this module needs, and the same
//! client serves every caller: the workstation launchers and the workflow
//! activities that will replace them. One client, not three.
//!
//! The service spoken to is configured, not named here: `JobsApi::new` takes
//! the API's base URL (for example `https://run.googleapis.com/v2`) and the
//! project and region the jobs live in.
//!
//! # Two rules the launchers learned the expensive way
//!
//! * **A read that hiccups is retried; a launch never is.** One
//!   `503 UNAVAILABLE` while following the whole flight on 23 September 2026
//!   killed the launcher as nine tasks carried on rendering. So a GET on 429,
//!   5xx or a dropped connection is retried with a doubling delay. A POST
//!   `:run`, retried, could start — and bill — a second execution: it fails
//!   the first time, loudly.
//! * **A token is refreshed before it dies, and once more on a 401.** An
//!   access token lives an hour and a render outlives it; the launcher died on
//!   `401` in the middle of a render on 16 September.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// What went wrong talking to the job service.
#[derive(Debug, thiserror::Error)]
pub enum JobsError {
    #[error("{method} {url} → {status}: {message}")]
    Http { method: &'static str, url: String, status: u16, message: String },
    #[error("{method} {url}: {message}")]
    Network { method: &'static str, url: String, message: String },
    #[error("authentication: {0}")]
    Auth(String),
    #[error("unexpected answer from {url}: {message}")]
    Shape { url: String, message: String },
}

pub type Result<T> = std::result::Result<T, JobsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One HTTP exchange. A trait so the retry and token rules can be tested
/// without a network; the real one is [`HttpTransport`].
#[async_trait]
pub trait Transport: Send + Sync {
    /// Status and body, or a network-level failure as text.
    async fn send(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        body: Option<Vec<u8>>,
    ) -> std::result::Result<(u16, Vec<u8>), String>;
}

/// [`Transport`] over `reqwest`.
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent("tuile-farm/0.1")
            .build()
            .map_err(|e| JobsError::Auth(format!("http client: {e}")))?;
        Ok(HttpTransport { client })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        body: Option<Vec<u8>>,
    ) -> std::result::Result<(u16, Vec<u8>), String> {
        let mut req = match method {
            Method::Get => self.client.get(url),
            Method::Post => self.client.post(url),
        }
        .bearer_auth(bearer);
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        Ok((status, bytes.to_vec()))
    }
}

/// Where access tokens come from.
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// A token valid for a while. `fresh` asks for a new one even if the
    /// cached one has not expired — the answer to a 401.
    async fn token(&self, fresh: bool) -> Result<String>;
}

/// A token and when it stops being worth using.
struct Cached {
    token: String,
    until: Instant,
}

/// Tokens minted from a service account key: a signed JWT exchanged for an
/// access token. What a worker in a cluster uses — no browser, no expiry
/// policy, no human.
pub struct ServiceAccountKey {
    key: KeyFile,
    http: reqwest::Client,
    cached: Mutex<Option<Cached>>,
}

#[derive(Deserialize)]
struct KeyFile {
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

impl ServiceAccountKey {
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| JobsError::Auth(format!("{}: {e}", path.display())))?;
        let key: KeyFile = serde_json::from_str(&text)
            .map_err(|e| JobsError::Auth(format!("{}: not a service account key: {e}", path.display())))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| JobsError::Auth(e.to_string()))?;
        Ok(ServiceAccountKey { key, http, cached: Mutex::new(None) })
    }

    async fn mint(&self) -> Result<Cached> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let claims = Claims {
            iss: &self.key.client_email,
            scope: "https://www.googleapis.com/auth/cloud-platform",
            aud: &self.key.token_uri,
            iat: now,
            exp: now + 3600,
        };
        let signer = jsonwebtoken::EncodingKey::from_rsa_pem(self.key.private_key.as_bytes())
            .map_err(|e| JobsError::Auth(format!("private key: {e}")))?;
        let assertion = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &signer,
        )
        .map_err(|e| JobsError::Auth(format!("signing: {e}")))?;
        let form: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
            .append_pair("assertion", &assertion)
            .finish();
        let resp = self
            .http
            .post(&self.key.token_uri)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .map_err(|e| JobsError::Auth(format!("token exchange: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(JobsError::Auth(format!("token exchange → {status}: {body}")));
        }
        #[derive(Deserialize)]
        struct Answer {
            access_token: String,
            expires_in: u64,
        }
        let answer: Answer = serde_json::from_str(&body)
            .map_err(|e| JobsError::Auth(format!("token exchange answer: {e}")))?;
        // Five minutes of margin: a token handed out one second before it
        // dies is a 401 waiting to happen.
        let life = Duration::from_secs(answer.expires_in.saturating_sub(300).max(60));
        Ok(Cached { token: answer.access_token, until: Instant::now() + life })
    }
}

#[async_trait]
impl TokenSource for ServiceAccountKey {
    async fn token(&self, fresh: bool) -> Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some(c) = cached.as_ref() {
            if !fresh && Instant::now() < c.until {
                return Ok(c.token.clone());
            }
        }
        let minted = self.mint().await?;
        let token = minted.token.clone();
        *cached = Some(minted);
        Ok(token)
    }
}

/// Tokens from the workstation's `gcloud` login. For a person at a terminal;
/// a worker uses [`ServiceAccountKey`].
pub struct GcloudToken {
    cached: Mutex<Option<Cached>>,
}

impl GcloudToken {
    pub fn new() -> Self {
        GcloudToken { cached: Mutex::new(None) }
    }
}

impl Default for GcloudToken {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TokenSource for GcloudToken {
    async fn token(&self, fresh: bool) -> Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some(c) = cached.as_ref() {
            if !fresh && Instant::now() < c.until {
                return Ok(c.token.clone());
            }
        }
        let out = tokio::process::Command::new("gcloud")
            .args(["auth", "print-access-token"])
            .output()
            .await
            .map_err(|e| JobsError::Auth(format!("gcloud: {e}")))?;
        if !out.status.success() {
            return Err(JobsError::Auth(format!(
                "gcloud returns no token — `gcloud auth login`, or point \
                 GOOGLE_APPLICATION_CREDENTIALS at a service account key.\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // gcloud does not say when its token dies; 45 minutes is inside the
        // hour it always gives.
        *cached = Some(Cached { token: token.clone(), until: Instant::now() + Duration::from_secs(45 * 60) });
        Ok(token)
    }
}

/// The key file named by `GOOGLE_APPLICATION_CREDENTIALS` if there is one,
/// the `gcloud` login otherwise.
pub fn default_tokens() -> Result<Arc<dyn TokenSource>> {
    match std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        Ok(path) if !path.is_empty() => {
            Ok(Arc::new(ServiceAccountKey::from_file(std::path::Path::new(&path))?))
        }
        _ => Ok(Arc::new(GcloudToken::new())),
    }
}

/// One execution of a job, as the service reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Execution {
    pub name: String,
    pub task_count: u32,
    pub running_count: u32,
    pub succeeded_count: u32,
    pub failed_count: u32,
    pub cancelled_count: u32,
    pub create_time: String,
    pub completion_time: Option<String>,
    pub log_uri: Option<String>,
}

impl Execution {
    /// The last path segment of the execution's name.
    pub fn short(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }
    pub fn done(&self) -> bool {
        self.completion_time.is_some()
    }
    /// Tasks that did not succeed: failed or cancelled.
    pub fn bad(&self) -> u32 {
        self.failed_count + self.cancelled_count
    }
    /// One readable line: `name  state  2✓ 0✗ 1⟳ of 3`.
    pub fn line(&self) -> String {
        let state = if self.done() {
            "finished"
        } else if self.running_count > 0 {
            "running"
        } else {
            "pending"
        };
        format!(
            "{}  {state}  {}✓ {}✗ {}⟳ of {}",
            self.short(),
            self.succeeded_count,
            self.bad(),
            self.running_count,
            self.task_count
        )
    }
}

/// HTTP statuses of a service that hiccuped rather than refused.
pub const TRANSIENT: [u16; 5] = [429, 500, 502, 503, 504];

/// The client.
pub struct JobsApi {
    transport: Arc<dyn Transport>,
    tokens: Arc<dyn TokenSource>,
    base: String,
    project: String,
    region: String,
    /// First retry delay; doubles up to a minute. Zero in tests.
    retry_base: Duration,
    /// Retries of a read before giving up.
    retries: u32,
}

impl JobsApi {
    pub fn new(
        transport: Arc<dyn Transport>,
        tokens: Arc<dyn TokenSource>,
        base: &str,
        project: &str,
        region: &str,
    ) -> Self {
        JobsApi {
            transport,
            tokens,
            base: base.trim_end_matches('/').to_string(),
            project: project.to_string(),
            region: region.to_string(),
            retry_base: Duration::from_secs(5),
            retries: 8,
        }
    }

    /// Retry pacing, for tests.
    pub fn with_retry(mut self, base: Duration, retries: u32) -> Self {
        self.retry_base = base;
        self.retries = retries;
        self
    }

    pub fn job_path(&self, job: &str) -> String {
        format!("projects/{}/locations/{}/jobs/{job}", self.project, self.region)
    }

    /// One call, with the two rules of the module doc.
    async fn call(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}/{}", self.base, path.trim_start_matches('/'))
        };
        let body = body.map(|b| b.to_string().into_bytes());
        let mut fresh = false;
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let token = self.tokens.token(fresh).await?;
            fresh = false;
            let retry = |attempt: u32| {
                self.retry_base.saturating_mul(1 << attempt.min(6)).min(Duration::from_secs(60))
            };
            match self.transport.send(method, &url, &token, body.clone()).await {
                Ok((status, bytes)) if (200..300).contains(&status) => {
                    // An empty 200 is a success, not a truncated JSON:
                    // `:cancel` sometimes answers with nothing.
                    if bytes.iter().all(u8::is_ascii_whitespace) {
                        return Ok(serde_json::Value::Object(Default::default()));
                    }
                    return serde_json::from_slice(&bytes)
                        .map_err(|e| JobsError::Shape { url, message: e.to_string() });
                }
                Ok((401, _)) if !refreshed => {
                    // Once, not in a loop: a token just renewed and refused
                    // again is a rights problem, and retrying would only hide it.
                    refreshed = true;
                    fresh = true;
                }
                Ok((status, _)) if TRANSIENT.contains(&status)
                    && method == Method::Get
                    && attempt < self.retries =>
                {
                    eprintln!("  (API {status}, retrying)");
                    tokio::time::sleep(retry(attempt)).await;
                    attempt += 1;
                }
                Ok((status, bytes)) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    let message = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                        .unwrap_or(text);
                    return Err(JobsError::Http { method: method.as_str(), url, status, message });
                }
                Err(message) if method == Method::Get && attempt < self.retries => {
                    eprintln!("  (network: {message}, retrying)");
                    tokio::time::sleep(retry(attempt)).await;
                    attempt += 1;
                }
                Err(message) => {
                    return Err(JobsError::Network { method: method.as_str(), url, message });
                }
            }
        }
    }

    /// The image the job will actually run — asked of the job rather than
    /// rebuilt by the caller, so a run's manifest says what ran.
    pub async fn job_image(&self, job: &str) -> Result<String> {
        let path = self.job_path(job);
        let v = self.call(Method::Get, &path, None).await?;
        v["template"]["template"]["containers"][0]["image"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| JobsError::Shape { url: path, message: "the job declares no image".into() })
    }

    /// Starts an execution of `job` with `env` merged into every task's
    /// environment and `tasks` tasks. With `validate_only`, the service checks
    /// the request and runs nothing: `None`.
    ///
    /// Every task receives the SAME environment — the API has no per-task
    /// override — which is why a job computes its slice from its task index.
    pub async fn run(
        &self,
        job: &str,
        env: &[(String, String)],
        tasks: u32,
        validate_only: bool,
    ) -> Result<Option<String>> {
        let env: Vec<_> = env.iter().map(|(k, v)| serde_json::json!({"name": k, "value": v})).collect();
        let mut body = serde_json::json!({
            "overrides": {"containerOverrides": [{"env": env}], "taskCount": tasks}
        });
        if validate_only {
            body["validateOnly"] = serde_json::Value::Bool(true);
        }
        let path = format!("{}:run", self.job_path(job));
        let op = self.call(Method::Post, &path, Some(body)).await?;
        if validate_only {
            return Ok(None);
        }
        // The answer is an Operation; the execution being created is named in
        // its metadata. Not awaited: it exists, and its name is what we need.
        op["metadata"]["name"]
            .as_str()
            .or_else(|| op["name"].as_str())
            .filter(|s| !s.is_empty())
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| JobsError::Shape { url: path, message: format!("no execution named in {op}") })
    }

    /// The digest behind an image tag in an artifact registry
    /// (`LOCATION-docker.pkg.dev/PROJECT/REPO/IMAGE:TAG`), asked of the
    /// registry's API. A tag moves — `5.1-su` named three different binaries
    /// in one week — so a manifest records the digest.
    pub async fn image_digest(&self, image: &str) -> Result<String> {
        let shape = || JobsError::Shape { url: image.to_string(), message: "not LOCATION-docker.pkg.dev/PROJECT/REPO/IMAGE:TAG".into() };
        let (host, rest) = image.split_once('/').ok_or_else(shape)?;
        let location = host.strip_suffix("-docker.pkg.dev").ok_or_else(shape)?;
        let (path, tag) = rest.rsplit_once(':').ok_or_else(shape)?;
        let mut parts = path.splitn(3, '/');
        let (project, repo, package) = match (parts.next(), parts.next(), parts.next()) {
            (Some(p), Some(r), Some(i)) => (p, r, i),
            _ => return Err(shape()),
        };
        let url = format!(
            "https://artifactregistry.googleapis.com/v1/projects/{project}/locations/{location}/repositories/{repo}/packages/{}/tags/{tag}",
            package.replace('/', "%2F")
        );
        let v = self.call(Method::Get, &url, None).await?;
        v["version"]
            .as_str()
            .and_then(|version| version.rsplit('/').next())
            .map(str::to_string)
            .ok_or_else(|| JobsError::Shape { url, message: "no version for this tag".into() })
    }

    /// Cancels execution `name`: its tasks stop and stop billing. For an
    /// orchestrator that launched work whose input will now never come.
    ///
    /// Not retried, like [`JobsApi::run`]: a second cancel of an execution
    /// already stopping is an error the caller should see, not paper over.
    pub async fn cancel(&self, name: &str) -> Result<()> {
        self.call(Method::Post, &format!("{name}:cancel"), Some(serde_json::json!({}))).await?;
        Ok(())
    }

    pub async fn execution(&self, name: &str) -> Result<Execution> {
        let v = self.call(Method::Get, name, None).await?;
        serde_json::from_value(v).map_err(|e| JobsError::Shape { url: name.to_string(), message: e.to_string() })
    }

    /// Follows `name` until the service reports it complete — completion, not
    /// the first success: one task of three having finished says nothing about
    /// the film, and a task still running is still billing. `on_poll` sees
    /// every poll (a workflow heartbeats there) with whether anything changed.
    pub async fn wait(
        &self,
        name: &str,
        every: Duration,
        mut on_poll: impl FnMut(&Execution, bool) + Send,
    ) -> Result<Execution> {
        let mut last: Option<Execution> = None;
        loop {
            let ex = self.execution(name).await?;
            let changed = last.as_ref() != Some(&ex);
            on_poll(&ex, changed);
            if ex.done() {
                return Ok(ex);
            }
            last = Some(ex);
            tokio::time::sleep(every).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Answers from a script, one per call, and records what was asked.
    struct Scripted {
        answers: std::sync::Mutex<Vec<std::result::Result<(u16, String), String>>>,
        seen: std::sync::Mutex<Vec<(Method, String, String)>>,
    }

    impl Scripted {
        fn new(answers: Vec<std::result::Result<(u16, &str), &str>>) -> Arc<Self> {
            let mut answers: Vec<_> = answers
                .into_iter()
                .map(|a| a.map(|(s, b)| (s, b.to_string())).map_err(str::to_string))
                .collect();
            answers.reverse();
            Arc::new(Scripted { answers: std::sync::Mutex::new(answers), seen: Default::default() })
        }
        fn calls(&self) -> Vec<(Method, String, String)> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl Transport for Scripted {
        async fn send(
            &self,
            method: Method,
            url: &str,
            bearer: &str,
            _body: Option<Vec<u8>>,
        ) -> std::result::Result<(u16, Vec<u8>), String> {
            self.seen.lock().expect("lock").push((method, url.to_string(), bearer.to_string()));
            let next = self.answers.lock().expect("lock").pop().expect("script ran out");
            next.map(|(s, b)| (s, b.into_bytes()))
        }
    }

    /// Hands out `t1`, `t2`, … — a new one each time a fresh token is asked.
    struct Counting(AtomicU32);

    #[async_trait]
    impl TokenSource for Counting {
        async fn token(&self, fresh: bool) -> Result<String> {
            if fresh || self.0.load(Ordering::SeqCst) == 0 {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Ok(format!("t{}", self.0.load(Ordering::SeqCst)))
        }
    }

    fn api(t: &Arc<Scripted>) -> JobsApi {
        JobsApi::new(t.clone(), Arc::new(Counting(AtomicU32::new(0))), "https://jobs.test/v2", "p", "r")
            .with_retry(Duration::ZERO, 8)
    }

    const DONE: &str = r#"{"name":"projects/p/locations/r/jobs/j/executions/j-abc","taskCount":3,
        "succeededCount":3,"completionTime":"2026-09-23T12:00:00Z"}"#;

    #[tokio::test]
    async fn a_read_survives_hiccups() {
        let t = Scripted::new(vec![Ok((503, "{}")), Err("connection reset"), Ok((200, DONE))]);
        let ex = api(&t).execution("projects/p/locations/r/jobs/j/executions/j-abc").await.expect("read");
        assert_eq!(ex.succeeded_count, 3);
        assert_eq!(t.calls().len(), 3);
    }

    #[tokio::test]
    async fn a_launch_is_never_retried() {
        // Retrying `:run` could start — and bill — a second execution.
        let t = Scripted::new(vec![Ok((503, r#"{"error":{"message":"unavailable"}}"#))]);
        let err = api(&t).run("j", &[], 3, false).await.expect_err("must fail");
        assert!(matches!(err, JobsError::Http { status: 503, .. }), "{err}");
        assert_eq!(t.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_401_gets_one_fresh_token_and_no_more() {
        let t = Scripted::new(vec![Ok((401, "{}")), Ok((200, DONE))]);
        api(&t).execution("x").await.expect("second token works");
        let bearers: Vec<_> = t.calls().into_iter().map(|c| c.2).collect();
        assert_eq!(bearers, ["t1", "t2"]);

        let t = Scripted::new(vec![Ok((401, "{}")), Ok((401, "{}"))]);
        let err = api(&t).execution("x").await.expect_err("rights problem");
        assert!(matches!(err, JobsError::Http { status: 401, .. }), "{err}");
    }

    #[tokio::test]
    async fn run_names_the_execution_and_sends_the_env() {
        let t = Scripted::new(vec![Ok((200, r#"{"metadata":{"name":"projects/p/locations/r/jobs/j/executions/j-xyz"}}"#))]);
        let name = api(&t)
            .run("j", &[("JOB_FRAMES".into(), "1:4".into())], 2, false)
            .await
            .expect("run");
        assert_eq!(name.as_deref(), Some("projects/p/locations/r/jobs/j/executions/j-xyz"));
        assert_eq!(t.calls()[0].1, "https://jobs.test/v2/projects/p/locations/r/jobs/j:run");
    }

    #[tokio::test]
    async fn cancel_posts_to_the_execution_itself() {
        let t = Scripted::new(vec![Ok((200, "{}"))]);
        api(&t).cancel("projects/p/locations/r/jobs/j/executions/j-xyz").await.expect("cancel");
        let calls = t.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, Method::Post);
        assert_eq!(calls[0].1, "https://jobs.test/v2/projects/p/locations/r/jobs/j/executions/j-xyz:cancel");
    }

    #[tokio::test]
    async fn wait_returns_at_completion_not_at_the_first_success() {
        let one_of_three = r#"{"name":"e","taskCount":3,"succeededCount":1,"runningCount":2}"#;
        let t = Scripted::new(vec![Ok((200, one_of_three)), Ok((200, one_of_three)), Ok((200, DONE))]);
        let mut changes = 0;
        let mut polls = 0;
        let ex = api(&t)
            .wait("e", Duration::ZERO, |_, changed| {
                polls += 1;
                changes += changed as u32;
            })
            .await
            .expect("wait");
        assert!(ex.done());
        assert_eq!((polls, changes), (3, 2));
    }
}

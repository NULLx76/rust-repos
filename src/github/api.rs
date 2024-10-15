// Copyright (c) 2018 Pietro Albini <pietro@pietroalbini.org>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is furnished to do
// so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use crate::config::Config;
use crate::prelude::*;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::{header, Method, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::borrow::Cow;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

static USER_AGENT: &str = "rust-repos (https://github.com/rust-ops/rust-repos)";

static GRAPHQL_QUERY_REPOSITORIES: &str = "
query($ids: [ID!]!) {
    nodes(ids: $ids) {
        ... on Repository {
            id
            nameWithOwner
            defaultBranchRef {
                name
            }
            languages(first: 100, orderBy: { field: SIZE, direction: DESC }) {
                nodes {
                    name
                }
            }
        }
    }

    rateLimit {
        cost
    }
}
";

#[derive(Fail, Debug)]
#[fail(display = "internal github error: {:?}", _0)]
struct RetryRequest(StatusCode);

trait ResponseExt {
    fn handle_errors(self) -> Fallible<Self>
    where
        Self: Sized;
}

impl ResponseExt for Response {
    fn handle_errors(self) -> Fallible<Self> {
        let status = self.status();
        match status {
            StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT => Err(RetryRequest(status).into()),
            _ => Ok(self),
        }
    }
}

pub struct GitHubApi<'conf> {
    config: &'conf Config,
    client: Client,
    slow_down: Arc<AtomicBool>,
    concurrent_requests: Arc<AtomicUsize>,
}

impl<'conf> GitHubApi<'conf> {
    pub fn new(config: &'conf Config) -> Self {
        GitHubApi {
            config,
            client: Client::new(),
            slow_down: Arc::new(AtomicBool::new(false)),
            concurrent_requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn retry<T, F: Fn() -> Fallible<T>>(&self, f: F) -> Fallible<T> {
        let mut wait = Duration::from_secs(10);
        let mut first = true;

        loop {
            let concurrent = self.concurrent_requests.fetch_add(1, Ordering::SeqCst);
            debug!(
                "currently making {} concurrent requests to the GitHub API",
                concurrent + 1
            );
            let res = f();
            self.concurrent_requests.fetch_sub(1, Ordering::SeqCst);

            match res {
                Ok(res) => return Ok(res),
                Err(err) => {
                    let mut retry = false;
                    if let Some(error) = err.downcast_ref::<RetryRequest>() {
                        warn!(
                            "API call to GitHub returned status code {}, retrying in {} seconds",
                            error.0,
                            wait.as_secs()
                        );
                        retry = true;
                    } else if let Some(error) = err.downcast_ref::<reqwest::Error>() {
                        if error.is_timeout() {
                            warn!(
                                "API call to GitHub timed out, retrying in {} seconds",
                                wait.as_secs()
                            );
                            retry = true;
                        }
                    } else if let Some(error) = err.downcast_ref::<std::io::Error>() {
                        if error.kind() == std::io::ErrorKind::ConnectionReset {
                            warn!(
                                "connection to the API reset by peer, retrying in {} seconds",
                                wait.as_secs()
                            );
                            retry = true;
                        }
                    }

                    if !retry {
                        return Err(err);
                    }
                }
            }

            // Slow down only once per API call
            if first {
                self.slow_down.store(true, Ordering::SeqCst);
            }

            ::std::thread::sleep(wait);

            // Stop doubling the time after a few increments, to avoid waiting too long
            // This is still a request every ~10 minutes
            if wait.as_secs() < 640 {
                wait *= 2;
            }

            first = false;
        }
    }

    fn build_request(&self, method: Method, url: &str) -> RequestBuilder {
        let url = if !url.starts_with("https://") {
            Cow::Owned(format!("https://api.github.com/{}", url))
        } else {
            Cow::Borrowed(url)
        };

        self.client
            .request(method, url.as_ref())
            .header(
                header::AUTHORIZATION,
                format!("token {}", self.config.github_token),
            )
            .header(header::USER_AGENT, USER_AGENT)
    }

    fn graphql<T: DeserializeOwned, V: Serialize>(&self, query: &str, variables: V) -> Fallible<T> {
        self.retry(|| {
            let resp: GraphResponse<T> = self
                .build_request(Method::POST, "graphql")
                .json(&json!({
                    "query": query,
                    "variables": variables,
                }))
                .send()?
                .handle_errors()?
                .json()?;

            if let Some(data) = resp.data {
                if let Some(errors) = resp.errors {
                    for error in errors {
                        if let Some(ref type_) = error.type_ {
                            if type_ == "NOT_FOUND" {
                                debug!("ignored GraphQL error: {}", error.message);
                                continue;
                            }
                        }

                        warn!("non-fatal GraphQL error: {}", error.message);
                    }
                }

                Ok(data)
            } else if let Some(mut errors) = resp.errors {
                Err(err_msg(errors.pop().unwrap().message)
                    .context("GitHub GraphQL call failed")
                    .into())
            } else if let Some(message) = resp.message {
                if message.contains("abuse") {
                    warn!("triggered GitHub abuse detection systems");
                    Err(RetryRequest(StatusCode::TOO_MANY_REQUESTS).into())
                } else {
                    Err(err_msg(message)
                        .context("GitHub GraphQL call failed")
                        .into())
                }
            } else {
                Err(err_msg("empty GraphQL response"))
            }
        })
    }

    pub fn scrape_repositories(&self, since: usize) -> Fallible<Vec<Option<RestRepository>>> {
        self.retry(|| {
            let resp = self
                .build_request(Method::GET, &format!("repositories?since={}", since))
                .send()?
                .handle_errors()?;

            let status = resp.status();
            if status == StatusCode::OK {
                Ok(resp.json()?)
            } else {
                let error: GitHubError = resp.json()?;
                if error.message.contains("abuse") {
                    warn!("triggered GitHub abuse detection systems");
                    Err(RetryRequest(StatusCode::TOO_MANY_REQUESTS).into())
                } else {
                    Err(err_msg(error.message)
                        .context(format!(
                            "GitHub API call failed with status code: {}",
                            status
                        ))
                        .context(format!(
                            "failed to fetch GitHub repositories since ID {}",
                            since
                        ))
                        .into())
                }
            }
        })
    }

    pub fn load_repositories(&self, node_ids: &[String]) -> Fallible<Vec<Option<GraphRepository>>> {
        let data: GraphRepositories = self.graphql(
            GRAPHQL_QUERY_REPOSITORIES,
            json!({
                "ids": node_ids,
            }),
        )?;

        assert!(
            data.rate_limit.cost <= 1,
            "load repositories query too costly"
        );
        Ok(data.nodes)
    }

    pub fn file_exists(&self, repo: &GraphRepository, path: &str) -> Fallible<bool> {
        let url = format!(
            "https://raw.githubusercontent.com/{}/{}/{}",
            repo.name_with_owner,
            if let Some(ref_) = &repo.default_branch_ref {
                &ref_.name
            } else {
                "master"
            },
            path,
        );

        self.retry(|| {
            let resp = self
                .build_request(Method::GET, &url)
                .send()?
                .handle_errors()?;
            match resp.status() {
                StatusCode::OK => Ok(true),
                StatusCode::NOT_FOUND => Ok(false),
                status => Err(
                    err_msg(format!("GitHub API returned status code {}", status))
                        .context(format!(
                            "failed to fetch file {} from repo {}",
                            path, repo.name_with_owner,
                        ))
                        .into(),
                ),
            }
        })
    }

    pub fn should_slow_down(&self) -> bool {
        self.slow_down.swap(false, Ordering::SeqCst)
    }
}

#[derive(Deserialize)]
struct GitHubError {
    message: String,
    #[serde(rename = "type")]
    type_: Option<String>,
}

#[derive(Deserialize)]
pub struct RestRepository {
    pub id: usize,
    pub full_name: String,
    pub node_id: String,
    pub fork: bool,
}

#[derive(Deserialize)]
struct GraphResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GitHubError>>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct GraphRateLimit {
    cost: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRepositories {
    nodes: Vec<Option<GraphRepository>>,
    rate_limit: GraphRateLimit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRepository {
    pub id: String,
    pub name_with_owner: String,
    pub default_branch_ref: Option<GraphRef>,
    pub languages: GraphLanguages,
}

#[derive(Debug, Deserialize)]
pub struct GraphLanguages {
    pub nodes: Vec<Option<GraphLanguage>>,
}

#[derive(Debug, Deserialize)]
pub struct GraphLanguage {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct GraphRef {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GitHubErrorType {
    NotFound,
    Other(String),
}

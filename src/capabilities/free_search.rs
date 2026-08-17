use std::time::Duration;

use crate::capabilities::narration::stable_labeled;
use async_trait::async_trait;
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Tool, ToolExecutionResult, capabilities::Capability};
use everruns_provider::ToolCall;
use everruns_provider::tool_types::ToolHints;
use html_escape::decode_html_entities;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;

const USER_AGENT: &str = concat!(
    "yolop/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/everruns/yolop)"
);
const MAX_RESULTS: usize = 10;

#[derive(Debug, Deserialize)]
struct FreeWebSearchArgs {
    query: String,
    #[serde(default = "default_topic")]
    topic: SearchTopic,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_topic() -> SearchTopic {
    SearchTopic::General
}

fn default_limit() -> usize {
    5
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SearchTopic {
    General,
    Dev,
}

#[derive(Debug, Serialize)]
struct FreeWebSearchResponse {
    query: String,
    topic: String,
    disclaimer: &'static str,
    methodology: Vec<Methodology>,
    results: Vec<SearchResult>,
}

#[derive(Debug, Serialize)]
struct Methodology {
    source: &'static str,
    url: String,
    method: &'static str,
    status: SourceStatus,
    note: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceStatus {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: Option<String>,
    source: &'static str,
    provenance: &'static str,
}

pub struct FreeSearchCapability;

impl FreeSearchCapability {
    pub fn new() -> Self {
        Self
    }
}

impl Capability for FreeSearchCapability {
    fn id(&self) -> &str {
        "free_search"
    }

    fn name(&self) -> &str {
        "Free best-effort web search"
    }

    fn description(&self) -> &str {
        "Adds a no-key, best-effort web search tool that transparently fetches public search/source pages and returns grounded links."
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(FreeWebSearchTool::new())]
    }
}

struct SearchEndpoints {
    duckduckgo_html: &'static str,
    github_repositories: &'static str,
    npm_registry: &'static str,
    crates_io: &'static str,
}

impl SearchEndpoints {
    fn production() -> Self {
        Self {
            duckduckgo_html: "https://html.duckduckgo.com/html/?q={query}",
            github_repositories: "https://api.github.com/search/repositories?q={query}&per_page=5",
            npm_registry: "https://registry.npmjs.org/-/v1/search?text={query}&size=5",
            crates_io: "https://crates.io/api/v1/crates?q={query}&per_page=5",
        }
    }

    fn url(&self, template: &'static str, query: &str) -> String {
        template.replace("{query}", &urlencoding::encode(query))
    }
}

struct FreeWebSearchTool {
    client: Client,
    endpoints: SearchEndpoints,
}

impl FreeWebSearchTool {
    fn new() -> Self {
        Self::with_endpoints(SearchEndpoints::production())
    }

    fn with_endpoints(endpoints: SearchEndpoints) -> Self {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("free search HTTP client should build");
        Self { client, endpoints }
    }

    async fn run(&self, args: FreeWebSearchArgs) -> Result<FreeWebSearchResponse, String> {
        let query = args.query.trim();
        if query.is_empty() {
            return Err("query must not be empty".to_string());
        }

        let topic = args.topic;
        let limit = args.limit.clamp(1, MAX_RESULTS);
        let mut methodology = Vec::new();
        let mut results = Vec::new();

        let ddg_url = self.endpoints.url(self.endpoints.duckduckgo_html, query);
        match self.fetch_text(&ddg_url).await {
            Ok(html) => {
                methodology.push(Methodology {
                    source: "duckduckgo_html",
                    url: ddg_url,
                    method: "GET public DuckDuckGo HTML results page and parse result anchors/snippets",
                    status: SourceStatus::Succeeded,
                    note: "Best-effort scraping of a public HTML page; ranking and availability are DuckDuckGo-dependent and not exhaustive.",
                });
                results.extend(parse_duckduckgo_html(&html));
            }
            Err(_) => methodology.push(Methodology {
                source: "duckduckgo_html",
                url: ddg_url,
                method: "GET public DuckDuckGo HTML results page and parse result anchors/snippets",
                status: SourceStatus::Failed,
                note: "Best-effort scraping failed; this does not prove there are no web results.",
            }),
        }

        if topic == SearchTopic::Dev {
            self.search_dev_sources(query, &mut methodology, &mut results)
                .await;
        }

        dedupe_results(&mut results);
        results.truncate(limit);

        Ok(FreeWebSearchResponse {
            query: query.to_string(),
            topic: match topic {
                SearchTopic::General => "general".to_string(),
                SearchTopic::Dev => "dev".to_string(),
            },
            disclaimer: "Best-effort free search, not a definitive or exhaustive answer. Results come from public pages/APIs listed in methodology; verify important claims by fetching source URLs.",
            methodology,
            results,
        })
    }

    async fn search_dev_sources(
        &self,
        query: &str,
        methodology: &mut Vec<Methodology>,
        results: &mut Vec<SearchResult>,
    ) {
        let github_url = self
            .endpoints
            .url(self.endpoints.github_repositories, query);
        match self.fetch_text(&github_url).await {
            Ok(body) => {
                methodology.push(Methodology {
                    source: "github_repositories",
                    url: github_url,
                    method: "GET GitHub Search API for repositories",
                    status: SourceStatus::Succeeded,
                    note: "Uses GitHub's public API without authentication; may be rate-limited and covers GitHub repositories only.",
                });
                results.extend(parse_github_repositories(&body));
            }
            Err(_) => methodology.push(Methodology {
                source: "github_repositories",
                url: github_url,
                method: "GET GitHub Search API for repositories",
                status: SourceStatus::Failed,
                note: "Public GitHub API request failed or was rate-limited; this does not prove no repositories exist.",
            }),
        }

        let npm_url = self.endpoints.url(self.endpoints.npm_registry, query);
        match self.fetch_text(&npm_url).await {
            Ok(body) => {
                methodology.push(Methodology {
                    source: "npm_registry",
                    url: npm_url,
                    method: "GET npm registry search API",
                    status: SourceStatus::Succeeded,
                    note: "Uses npm's public registry search endpoint; covers npm packages only.",
                });
                results.extend(parse_npm_search(&body));
            }
            Err(_) => methodology.push(Methodology {
                source: "npm_registry",
                url: npm_url,
                method: "GET npm registry search API",
                status: SourceStatus::Failed,
                note: "npm registry search failed; this does not prove no packages exist.",
            }),
        }

        let crates_url = self.endpoints.url(self.endpoints.crates_io, query);
        match self.fetch_text(&crates_url).await {
            Ok(body) => {
                methodology.push(Methodology {
                    source: "crates_io",
                    url: crates_url,
                    method: "GET crates.io search API",
                    status: SourceStatus::Succeeded,
                    note: "Uses crates.io's public API; covers Rust crates only.",
                });
                results.extend(parse_crates_search(&body));
            }
            Err(_) => methodology.push(Methodology {
                source: "crates_io",
                url: crates_url,
                method: "GET crates.io search API",
                status: SourceStatus::Failed,
                note: "crates.io search failed; this does not prove no crates exist.",
            }),
        }
    }

    async fn fetch_text(&self, url: &str) -> Result<String, reqwest::Error> {
        self.client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await
    }
}

#[async_trait]
impl Tool for FreeWebSearchTool {
    fn name(&self) -> &str {
        "free_web_search"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Free Web Search")
    }

    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let detail = arg_str(&tool_call.arguments, &["query"]).map(|query| truncate(query, 48));
        Some(stable_labeled("Search web", detail, phase))
    }

    fn description(&self) -> &str {
        "Best-effort free web search. Fetches public DuckDuckGo HTML results and, for topic='dev', public developer source APIs (GitHub repositories, npm, crates.io). Not exhaustive; output includes methodology/source links so claims can be grounded."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query string."
                },
                "topic": {
                    "type": "string",
                    "enum": ["general", "dev"],
                    "default": "general",
                    "description": "Search topic. 'general' scrapes a public DuckDuckGo HTML result page. 'dev' also queries public developer sources such as GitHub repositories, npm, and crates.io."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 5,
                    "description": "Maximum number of deduplicated results to return."
                }
            }
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let args = match serde_json::from_value::<FreeWebSearchArgs>(arguments) {
            Ok(args) => args,
            Err(err) => {
                return ToolExecutionResult::tool_error(format!("invalid arguments: {err}"));
            }
        };

        match self.run(args).await {
            Ok(response) => ToolExecutionResult::success(json!(response)),
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default()
            .with_readonly(true)
            .with_idempotent(false)
    }
}

fn parse_duckduckgo_html(html: &str) -> Vec<SearchResult> {
    let anchor_re = Regex::new(
        r#"(?s)<a[^>]+class=\"result__a\"[^>]+href=\"(?P<href>[^\"]+)\"[^>]*>(?P<title>.*?)</a>"#,
    )
    .expect("valid DuckDuckGo result regex");
    let snippet_re =
        Regex::new(r#"(?s)<a[^>]+class=\"result__snippet\"[^>]*>(?P<snippet>.*?)</a>"#)
            .expect("valid DuckDuckGo snippet regex");
    let snippets: Vec<_> = snippet_re
        .captures_iter(html)
        .filter_map(|caps| caps.name("snippet"))
        .map(|m| clean_html_text(m.as_str()))
        .collect();

    anchor_re
        .captures_iter(html)
        .enumerate()
        .filter_map(|(index, caps)| {
            let title = clean_html_text(caps.name("title")?.as_str());
            let href = decode_duckduckgo_href(caps.name("href")?.as_str());
            if title.is_empty() || href.is_empty() {
                return None;
            }
            Some(SearchResult {
                title,
                url: href,
                snippet: snippets.get(index).cloned().filter(|s| !s.is_empty()),
                source: "duckduckgo_html",
                provenance: "scraped from public DuckDuckGo HTML result page",
            })
        })
        .collect()
}

fn parse_github_repositories(body: &str) -> Vec<SearchResult> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    json.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(SearchResult {
                title: item.get("full_name")?.as_str()?.to_string(),
                url: item.get("html_url")?.as_str()?.to_string(),
                snippet: item
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                source: "github_repositories",
                provenance: "GitHub public repository search API",
            })
        })
        .collect()
}

fn parse_npm_search(body: &str) -> Vec<SearchResult> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    json.get("objects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|object| {
            let package = object.get("package")?;
            let name = package.get("name")?.as_str()?;
            Some(SearchResult {
                title: name.to_string(),
                url: package
                    .get("links")
                    .and_then(|links| links.get("npm"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("https://www.npmjs.com/package/{name}")),
                snippet: package
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                source: "npm_registry",
                provenance: "npm public registry search API",
            })
        })
        .collect()
}

fn parse_crates_search(body: &str) -> Vec<SearchResult> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    json.get("crates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|krate| {
            let name = krate.get("id")?.as_str()?;
            Some(SearchResult {
                title: name.to_string(),
                url: format!("https://crates.io/crates/{name}"),
                snippet: krate
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                source: "crates_io",
                provenance: "crates.io public search API",
            })
        })
        .collect()
}

fn dedupe_results(results: &mut Vec<SearchResult>) {
    let mut seen = std::collections::HashSet::new();
    results.retain(|result| seen.insert(normalize_url_for_dedupe(&result.url)));
}

fn normalize_url_for_dedupe(url: &str) -> String {
    url.trim_end_matches('/').to_ascii_lowercase()
}

fn decode_duckduckgo_href(href: &str) -> String {
    let decoded = decode_html_entities(href).to_string();
    if let Some(query_start) = decoded.find("uddg=") {
        let value_start = query_start + "uddg=".len();
        let value_end = decoded[value_start..]
            .find('&')
            .map(|offset| value_start + offset)
            .unwrap_or(decoded.len());
        return percent_decode(&decoded[value_start..value_end]);
    }
    decoded
}

fn percent_decode(value: &str) -> String {
    urlencoding::decode(value)
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

fn clean_html_text(value: &str) -> String {
    let tag_re = Regex::new(r"(?s)<[^>]+>").expect("valid tag stripping regex");
    let without_tags = tag_re.replace_all(value, " ");
    decode_html_entities(&without_tags)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_runs_search_through_tool_entrypoint() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("test server starts");
        let base_url = format!("http://{}", server.server_addr());
        let server_thread = std::thread::spawn(move || {
            for request in server.incoming_requests().take(4) {
                let url = request.url().to_string();
                let body = if url.starts_with("/duck") {
                    r#"<a class="result__a" href="https://bashkit.dev/">Bashkit</a><a class="result__snippet">Agent SDK docs</a>"#
                } else if url.starts_with("/github") {
                    r#"{"items":[{"full_name":"everruns/bashkit","html_url":"https://github.com/everruns/bashkit","description":"Virtual Bash interpreter"}]}"#
                } else if url.starts_with("/npm") {
                    r#"{"objects":[{"package":{"name":"bashkit","description":"Claude Agents SDK","links":{"npm":"https://www.npmjs.com/package/bashkit"}}}]}"#
                } else if url.starts_with("/crates") {
                    r#"{"crates":[{"id":"bashkit","description":"Bash toolkit"}]}"#
                } else {
                    "{}"
                };
                request
                    .respond(tiny_http::Response::from_string(body))
                    .expect("response sends");
            }
        });

        let tool = FreeWebSearchTool::with_endpoints(SearchEndpoints {
            duckduckgo_html: Box::leak(format!("{base_url}/duck?q={{query}}").into_boxed_str()),
            github_repositories: Box::leak(
                format!("{base_url}/github?q={{query}}").into_boxed_str(),
            ),
            npm_registry: Box::leak(format!("{base_url}/npm?q={{query}}").into_boxed_str()),
            crates_io: Box::leak(format!("{base_url}/crates?q={{query}}").into_boxed_str()),
        });

        let result = tokio::runtime::Runtime::new()
            .expect("runtime starts")
            .block_on(tool.execute(json!({
                "query": "bashkit",
                "topic": "dev",
                "limit": 10
            })));
        server_thread.join().expect("server thread exits");

        let output = match result {
            ToolExecutionResult::Success(output) => output,
            other => panic!("tool should succeed: {other:?}"),
        };
        assert_eq!(output["query"], "bashkit");
        assert!(
            output["disclaimer"]
                .as_str()
                .expect("disclaimer is text")
                .contains("not a definitive or exhaustive answer")
        );
        let sources: std::collections::HashSet<_> = output["results"]
            .as_array()
            .expect("results are an array")
            .iter()
            .filter_map(|result| result["source"].as_str())
            .collect();
        assert!(sources.contains("duckduckgo_html"));
        assert!(sources.contains("github_repositories"));
        assert!(sources.contains("npm_registry"));
        assert!(sources.contains("crates_io"));
        assert_eq!(
            output["methodology"]
                .as_array()
                .expect("methodology is an array")
                .len(),
            4
        );
    }

    #[test]
    fn parses_duckduckgo_result_links_and_snippets() {
        let html = r#"
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fbashkit.dev%2F&amp;rut=abc">Bashkit &amp; docs</a>
            <a class="result__snippet">The <b>Claude Agents SDK</b> for Vercel AI SDK</a>
        "#;

        let results = parse_duckduckgo_html(html);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Bashkit & docs");
        assert_eq!(results[0].url, "https://bashkit.dev/");
        assert_eq!(
            results[0].snippet.as_deref(),
            Some("The Claude Agents SDK for Vercel AI SDK")
        );
        assert_eq!(results[0].source, "duckduckgo_html");
    }

    #[test]
    fn parses_developer_source_results() {
        let github = r#"{"items":[{"full_name":"everruns/bashkit","html_url":"https://github.com/everruns/bashkit","description":"Virtual Bash interpreter"}]}"#;
        let npm = r#"{"objects":[{"package":{"name":"bashkit","description":"Claude Agents SDK","links":{"npm":"https://www.npmjs.com/package/bashkit"}}}]}"#;
        let crates = r#"{"crates":[{"id":"bashkit","description":"Bash toolkit"}]}"#;

        let github_results = parse_github_repositories(github);
        let npm_results = parse_npm_search(npm);
        let crates_results = parse_crates_search(crates);

        assert_eq!(github_results[0].url, "https://github.com/everruns/bashkit");
        assert_eq!(npm_results[0].url, "https://www.npmjs.com/package/bashkit");
        assert_eq!(crates_results[0].url, "https://crates.io/crates/bashkit");
    }

    #[test]
    fn dedupes_case_and_trailing_slashes() {
        let mut results = vec![
            SearchResult {
                title: "A".into(),
                url: "https://Example.com/Thing/".into(),
                snippet: None,
                source: "duckduckgo_html",
                provenance: "test",
            },
            SearchResult {
                title: "B".into(),
                url: "https://example.com/thing".into(),
                snippet: None,
                source: "github_repositories",
                provenance: "test",
            },
        ];

        dedupe_results(&mut results);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "A");
    }
}

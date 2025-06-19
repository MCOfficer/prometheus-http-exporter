use anyhow::Result;
use anyhow::{anyhow, Context};
use regex::Regex;
use reqwest::Client;
#[cfg(feature = "generate-schema")]
use schemars::JsonSchema;
use serde::Deserialize;
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

static CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

#[derive(Default)]
struct Metrics {
    gauges: HashSet<Metric>,
}

#[derive(Default, Clone, Debug)]
struct Metric {
    name: String,
    value: f64,
    timestamp: Option<u128>,
    labels: HashMap<String, String>,
    rendered: Option<String>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
struct Config {
    /// How verbose logging should be
    #[serde(default = "default_log_level")]
    log_level: String,
    /// The address to bind to.
    #[serde(default = "default_address")]
    address: String,
    /// Scrapes each target while starting up. Useful to test your config, don't use in production.
    #[serde(default)]
    scrape_on_startup: bool,
    targets: Arc<Vec<Target>>,
}

fn default_log_level() -> String {
    "info".into()
}
fn default_address() -> String {
    "0.0.0.0:3000".into()
}

#[derive(Deserialize, Debug)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
struct Target {
    /// The target's name. Must be unique, must not contain newlines.
    name: String,
    /// The URL that should be fetched.
    url: String,
    /// Additional headers. User-Agent is set by default.
    #[serde(default)]
    headers: HashMap<String, String>,
    /// When the job should run. Supported formats: [english-to-cron](https://github.com/kaplanelad/english-to-cron#full-list-of-supported-english-patterns), [croner](https://github.com/Hexagon/croner-rust#pattern)
    cron: String,
    #[serde(default)]
    extractor: Extractor,
    /// A set of rules
    rules: Vec<Rule>,
}

/// Which engine shall be used to process the response.
#[derive(Deserialize, Clone, Default, Debug)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
#[serde(rename_all = "lowercase")]
enum Extractor {
    #[default]
    Jq,
    Regex,
}

/// How to process to fetched data into metrics.
#[derive(Deserialize, Debug)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
struct Rule {
    /// The rule's name, and that of any metrics generated. Should be snake_case to conform with Prometheus specs.
    name: String,
    /// Instructions for the selected extractor, f.e. a jq query or regex pattern.
    extract: String,
    #[serde(skip)]
    extractor_storage: Mutex<ExtractorStorage>,
    #[serde(skip)]
    results: Mutex<Vec<Metric>>,
}

#[derive(Default)]
struct ExtractorStorage {
    jq_filter: Option<jq::JsonFilter>,
    regex: Option<Regex>,
}
impl std::fmt::Debug for ExtractorStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractorStorage")
            .field("regex", &self.regex)
            .field("jq_filter", &"(not impl Debug)")
            .finish_non_exhaustive()
    }
}

/// The type of prometheus metric.
#[derive(Deserialize, Clone, Default)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
#[serde(rename_all = "lowercase")]
enum MetricType {
    #[default]
    Gauge,
}

#[tokio::main]
async fn main() {
    #[cfg(feature = "generate-schema")]
    {
        let schema = schemars::schema_for!(Config);
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .as_path()
            .join("config.schema.json");
        std::fs::write(path, serde_json::to_string_pretty(&schema).unwrap()).unwrap();
        return;
    }

    let config_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("Usage: prometheus-http-exporter <path to config.yml>",))
        .unwrap();
    let config_file = File::open(config_path)
        .context("Failed to open config file")
        .unwrap();
    let config: Config = serde_yml::from_reader(config_file)
        .context("Failed to Deserialize config")
        .unwrap();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(
            Level::from_str(&config.log_level)
                .unwrap_or_else(|_| panic!("Invalid log level '{}'", config.log_level)),
        )
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    for target in config.targets.iter() {
        target.setup().await.unwrap()
    }

    let listener = tokio::net::TcpListener::bind(&config.address)
        .await
        .with_context(|| format!("binding to {}", config.address))
        .unwrap();

    if config.scrape_on_startup {
        info!("Initial Scraping of {} targets", config.targets.len());
        warn!(
            "Initial Scraping is supposed to be used for testing purposes. Any error will result in a shutdown."
        );
        for target in config.targets.iter() {
            info!(name = target.name, "Scraping");
            try_scrape_target(target).await.unwrap();
            for rule in &target.rules {
                let results = rule.results.lock().await;
                for metric in results.iter() {
                    info!("=> {}", metric.render())
                }
            }
        }
    }

    let scheduler = JobScheduler::new()
        .await
        .with_context(|| "creating job scheduler")
        .unwrap();
    for (index, target) in config.targets.iter().enumerate() {
        info!(target = target.name, cron = target.cron, "Creating job");
        let targets = config.targets.clone();
        let job = Job::new_async(target.cron.clone(), move |uuid, mut l| {
            let targets_clone = targets.clone();
            Box::pin({
                async move {
                    let target = targets_clone.get(index).unwrap();
                    if let Err(e) = try_scrape_target(target).await {
                        error!("{}: {:#?}", &target.name, e);
                    }
                    if let Some(n) = l.next_tick_for_job(uuid).await.ok().flatten() {
                        debug!("{}: next run {}", &target.name, n)
                    }
                }
            })
        })
        .unwrap();
        scheduler
            .add(job)
            .await
            .with_context(|| "adding job to scheduler")
            .unwrap();
    }
    scheduler
        .start()
        .await
        .with_context(|| "starting scheduler")
        .unwrap();

    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get({
            let targets = config.targets.clone();
            move || serve_metrics(targets)
        }),
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler")
        })
        .await
        .unwrap()
}

async fn serve_metrics(targets: Arc<Vec<Target>>) -> impl axum::response::IntoResponse {
    let mut lines = vec![];

    for target in targets.iter() {
        lines.push(format!(
            "################### {} ###################\n",
            target.name
        ));
        for rule in &target.rules {
            let results: Vec<_> = rule
                .results
                .lock()
                .await
                .iter()
                .map(|m| m.render())
                .collect();
            if !results.is_empty() {
                lines.push(format!("# TYPE {} gauge", rule.name));
                lines.extend_from_slice(&results);
            }
            lines.push(String::default());
        }
        lines.push(String::default());
    }

    let body = lines.join("\n");
    let headers = [(
        axum::http::header::CONTENT_TYPE,
        "text/plain; version=0.0.4",
    )];
    (headers, body)
}

async fn try_scrape_target(target: &Target) -> Result<()> {
    let mut builder = CLIENT.get(&target.url);

    if !target.headers.contains_key("User-Agent") {
        builder = builder.header(
            "User-Agent",
            format!(
                "{}/{} ({})",
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_REPOSITORY")
            ),
        );
    }

    for (k, v) in &target.headers {
        builder = builder.header(k, v)
    }
    let request = builder.build().with_context(|| "building request")?;
    debug!(
        target = target.name,
        url = request.url().as_str(),
        "Fetching"
    );
    let response = CLIENT
        .execute(request)
        .await
        .with_context(|| "requesting")?
        .error_for_status()
        .with_context(|| "status code")?
        .text()
        .await
        .with_context(|| "parsing response as string")?;

    target.extract(response).await?;
    Ok(())
}

impl Target {
    async fn setup(&self) -> Result<()> {
        info!("Setting up Extractors for Target \"{}\"", self.name);
        for rule in &self.rules {
            info!("=> {}", rule.name);
            match self.extractor {
                Extractor::Jq => {
                    let filter = jq::JsonFilter::from_str(&rule.extract)
                        .map_err(|e| anyhow!("Failed to compile jq filter: {:#?}", e))?;
                    rule.extractor_storage.lock().await.jq_filter = Some(filter);
                }
                Extractor::Regex => {
                    let regex =
                        Regex::new(&rule.extract).with_context(|| "Failed to compile regex")?;
                    rule.extractor_storage.lock().await.regex = Some(regex);
                }
            }
        }
        Ok(())
    }
    async fn extract(&self, text: String) -> Result<()> {
        debug!(target = self.name, "Extracting from response");
        for rule in &self.rules {
            let mut to_save = vec![];
            match self.extractor {
                Extractor::Jq => {
                    debug!(target = self.name, rule = rule.name, "Processing with jq");
                    let lock = rule.extractor_storage.lock().await;
                    let value = lock
                        .jq_filter
                        .as_ref()
                        .unwrap()
                        .filter_json_str(&text)
                        .map_err(|e| match e {
                            jq::JsonFilterError::Execute(e) => anyhow!("JQ Error: {}", e),
                            _ => anyhow!("JQ Error: {:#?}", e),
                        })?;

                    if let Some(obj) = value.as_object() {
                        debug!(
                            target = self.name,
                            rule = rule.name,
                            "jq produced an object, treating it as multiple metrics"
                        );
                        for (key, value) in obj {
                            if let Some(num) = value.as_f64() {
                                Metric::new(&rule.name, num)
                                    .with_label("key", key)
                                    .insert(&mut to_save)
                                    .await;
                            }
                        }
                    } else if let Some(arr) = value.as_array() {
                        debug!(
                            target = self.name,
                            rule = rule.name,
                            "jq produced an array, treating it as multiple metrics"
                        );
                        for obj in arr {
                            if let Some(obj) = obj.as_object() {
                                if let Some(value) = obj.get("value") {
                                    if let Some(num) = value.as_f64() {
                                        let metric = Metric::new(&rule.name, num);
                                        obj.iter()
                                            .filter(|(_, v)| {
                                                v.is_number() || v.is_string() || v.is_boolean()
                                            })
                                            .map(|(k, v)| (k, v.to_string()))
                                            .fold(metric, |m, (k, v)| {
                                                m.with_label(k, v.trim_matches('"'))
                                            })
                                            .insert(&mut to_save)
                                            .await;
                                    }
                                }
                            }
                        }
                    } else if let Some(num) = value.as_f64() {
                        debug!(target = self.name, rule = rule.name, "jq produced a number");
                        // for numbers, as_f64 is always Some
                        Metric::new(&rule.name, num).insert(&mut to_save).await;
                    }
                }

                Extractor::Regex => {
                    debug!(
                        target = self.name,
                        rule = rule.name,
                        "Processing with regex"
                    );
                    let lock = rule.extractor_storage.lock().await;
                    let regex = lock.regex.as_ref().unwrap();
                    let names: Vec<_> = regex.capture_names().flatten().collect();
                    let captures = regex
                        .captures(&text)
                        .with_context(|| "Regex didn't match anything")?;
                    if names.contains(&"value") {
                        let group = captures.name("value").with_context(
                            || "Named group 'value' exists, but didn't match anything?",
                        )?;
                        if let Ok(num) = group.as_str().parse::<f64>() {
                            let metric = Metric::new(&rule.name, num);
                            names
                                .iter()
                                .filter_map(|name| captures.name(name).map(|m| (name, m.as_str())))
                                .fold(metric, |m, (name, matched)| m.with_label(*name, matched))
                                .insert(&mut to_save)
                                .await;
                        }
                    } else {
                        // There are no (relevant) named groups - concat the remaining groups.
                        // If there are no groups use the entire match (group 0).
                        let captures: Vec<_> = captures
                            .iter()
                            .flatten()
                            .skip(min(1, captures.len() - 1))
                            .map(|m| m.as_str())
                            .collect();
                        let string = captures.join("");
                        let num = string
                            .parse::<f64>()
                            .with_context(|| format!("Regex matched, but the result could not be parsed as f64: '{string}'"))?;
                        Metric::new(&rule.name, num).insert(&mut to_save).await;
                    }
                }
            }
            if !to_save.is_empty() {
                *rule.results.lock().await = to_save;
            }
        }
        Ok(())
    }
}

impl Metric {
    fn new<N, V>(name: N, value: V) -> Self
    where
        N: Into<String>,
        V: Into<f64>,
    {
        let timestamp = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before UNIX_EPOCH??")
                .as_millis(),
        );
        Self {
            name: name.into(),
            value: value.into(),
            timestamp,
            ..Default::default()
        }
    }

    fn with_label<K, V>(mut self, key: K, value: V) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.labels.insert(
            sanitize_for_prometheus(key.as_ref()),
            sanitize_for_prometheus(value.as_ref()),
        );
        self
    }

    async fn insert(mut self, vec: &mut Vec<Metric>) {
        debug!(
            name = self.name,
            value = self.value,
            labels = self.labels.len(),
            "Saving new metric"
        );
        self.rendered = Some(self.render());
        vec.push(self);
    }

    fn render(&self) -> String {
        if let Some(rendered) = &self.rendered {
            return rendered.clone();
        }

        let name = &self.name;
        let value = self.value;

        let labels = self
            .labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .reduce(|a, b| format!("{a},{b}"))
            .map(|l| format!("{{{l}}}"))
            .unwrap_or_default();

        let timestamp = match self.timestamp {
            None => "",
            Some(t) => &t.to_string(),
        };

        format!("{name}{labels} {value} {timestamp}")
    }
}

fn sanitize_for_prometheus(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

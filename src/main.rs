use anyhow::Result;
use anyhow::{anyhow, Context};
use convert_case::{Case, Casing};
use regex::Regex;
use reqwest::Client;
#[cfg(feature = "generate-schema")]
use schemars::JsonSchema;
use serde::Deserialize;
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

static METRICS: LazyLock<Mutex<Metrics>> = LazyLock::new(|| Mutex::new(Metrics::default()));
static CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

#[derive(Default)]
struct Metrics {
    gauges: HashSet<Metric>,
}

#[derive(Default)]
struct Metric {
    name: String,
    value: f64,
    timestamp: Option<u128>,
    labels: HashMap<String, String>,
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
    targets: Vec<Target>,
}

fn default_log_level() -> String {
    "info".into()
}
fn default_address() -> String {
    "0.0.0.0:3000".into()
}

#[derive(Deserialize, Clone)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
struct Target {
    /// The target's name. Must be unique.
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
#[derive(Deserialize, Clone, Default)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
#[serde(rename_all = "lowercase")]
enum Extractor {
    #[default]
    Jq,
    Regex,
}

/// How to process to fetched data into metrics.
#[derive(Deserialize, Clone)]
#[cfg_attr(feature = "generate-schema", derive(JsonSchema))]
struct Rule {
    /// The rule's name, and that of any metrics generated. Should be snake_case to conform with Prometheus specs.
    name: String,
    /// Instructions for the selected extractor, f.e. a jq query or regex pattern.
    extract: String,
    #[serde(skip)]
    extractor_storage: ExtractorStorage,
}

#[derive(Clone, Default)]
struct ExtractorStorage {
    jq_filter: Option<jq::JsonFilter>,
    regex: Option<Regex>,
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
    let mut config: Config = serde_yml::from_reader(config_file)
        .context("Failed to Deserialize config")
        .unwrap();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(
            Level::from_str(&config.log_level)
                .unwrap_or_else(|_| panic!("Invalid log level '{}'", config.log_level)),
        )
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let scheduler = JobScheduler::new()
        .await
        .with_context(|| "creating job scheduler")
        .unwrap();
    for target in &mut config.targets {
        target.setup().unwrap();
    }

    let listener = tokio::net::TcpListener::bind(&config.address)
        .await
        .with_context(|| format!("binding to {}", config.address))
        .unwrap();

    if config.scrape_on_startup {
        info!("Initial Scraping of {} targets", config.targets.len());
        for target in &config.targets {
            info!("Scraping {}...", target.name);
            let before = METRICS.lock().await.gauges.len();
            try_scrape_target(target).await.unwrap();
            let total = METRICS.lock().await.gauges.len() - before;
            info!("=> scraped {total} metrics")
        }
    }

    for target in config.targets {
        let job = Job::new_async(target.cron.clone(), move |uuid, mut l| {
            Box::pin({
                let target_clone = target.clone();
                async move {
                    if let Err(e) = try_scrape_target(&target_clone).await {
                        error!("{}: {:#?}", &target_clone.name, e);
                    }
                    if let Some(n) = l.next_tick_for_job(uuid).await.ok().flatten() {
                        debug!("{}: next run {}", &target_clone.name, n)
                    }
                }
            })
        })
        .with_context(|| "creating job for target")
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

    let app = axum::Router::new().route("/metrics", axum::routing::get(serve_metrics));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler")
        })
        .await
        .unwrap()
}

async fn serve_metrics() -> impl axum::response::IntoResponse {
    let mut parts: Vec<String> = METRICS
        .lock()
        .await
        .gauges
        .iter()
        .map(
            |Metric {
                 name,
                 value,
                 timestamp,
                 labels,
             }| {
                format!(
                    "# TYPE gauge\n{name} {value} {}",
                    timestamp.unwrap_or_default()
                )
            },
        )
        .collect();
    parts.sort_unstable();
    let body = parts.join("\n\n");
    let headers = [(
        axum::http::header::CONTENT_TYPE,
        "text/plain; version=0.0.4",
    )];
    (headers, body)
}

async fn try_scrape_target(target: &Target) -> Result<()> {
    let mut builder = CLIENT.get(&target.url).header(
        "User-Agent",
        format!(
            "{}/{} ({})",
            env!("CARGO_CRATE_NAME"),
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY")
        ),
    );
    for (k, v) in &target.headers {
        builder = builder.header(k, v)
    }
    let request = builder.build().with_context(|| "building request")?;
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
    fn setup(&mut self) -> Result<()> {
        info!("Setting up Extractors for Target \"{}\"", self.name);
        for rule in &mut self.rules {
            info!("=> {}", rule.name);
            match self.extractor {
                Extractor::Jq => {
                    let filter = jq::JsonFilter::from_str(&rule.extract)
                        .map_err(|e| anyhow!("Failed to compile jq filter: {:#?}", e))?;
                    rule.extractor_storage.jq_filter = Some(filter);
                }
                Extractor::Regex => {
                    let regex =
                        Regex::new(&rule.extract).with_context(|| "Failed to compile regex")?;
                    rule.extractor_storage.regex = Some(regex);
                }
            }
        }
        Ok(())
    }

    async fn extract(&self, text: String) -> Result<()> {
        debug!("Extracting from response for {}", self.name);
        for rule in &self.rules {
            debug!("Processing rule {}", &rule.name);
            let to_rule_name = |name: &str| {
                format!(
                    "{}_{}",
                    rule.name,
                    name.to_case(Case::Snake).to_ascii_lowercase()
                )
            };

            match self.extractor {
                Extractor::Jq => {
                    let value = rule
                        .extractor_storage
                        .jq_filter
                        .as_ref()
                        .unwrap()
                        .filter_json_str(&text)
                        .map_err(|e| match e {
                            jq::JsonFilterError::Execute(e) => anyhow!("JQ Error: {}", e),
                            _ => anyhow!("JQ Error: {:#?}", e),
                        })?;

                    if let Some(obj) = value.as_object() {
                        for (key, value) in obj {
                            if let Some(num) = value.as_f64() {
                                Metric::new(&rule.name, num)
                                    .with_label("key", key)
                                    .save()
                                    .await;
                            }
                        }
                    } else if let Some(arr) = value.as_array() {
                        for obj in arr {
                            if let Some(value) = obj.get("value") {
                                if let Some(num) = value.as_f64() {
                                    Metric::new(&rule.name, num).save().await;
                                }
                            }
                        }
                    } else if let Some(num) = value.as_f64() {
                        // for numbers, as_f64 is always Some
                        Metric::new(&rule.name, num).save().await;
                    }
                }

                Extractor::Regex => {
                    let regex = rule.extractor_storage.regex.as_ref().unwrap();
                    let names: Vec<_> = regex.capture_names().flatten().collect();
                    let captures = regex
                        .captures(&text)
                        .with_context(|| "Regex didn't match anything")?;
                    if names.is_empty() {
                        // If there are no named groups, use the first group. Failing that, use the entire match
                        let group = captures
                            .iter()
                            .flatten()
                            .nth(min(1, captures.len() - 1))
                            .unwrap();
                        if let Ok(num) = group.as_str().parse::<f64>() {
                            Metric::new(&rule.name, num).save().await;
                        }
                    } else if let Some(group) = captures.name("value") {
                        if let Ok(num) = group.as_str().parse::<f64>() {
                            let metric = Metric::new(&rule.name, num);
                            names
                                .iter()
                                .filter_map(|name| captures.name(name).map(|m| (name, m.as_str())))
                                .fold(metric, |m, (name, matched)| m.with_label(*name, matched))
                                .save()
                                .await;
                        }
                    }
                }
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
            labels: Default::default(),
        }
    }

    fn with_label<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        // TODO: escaping etc
        self.labels.insert(key.into(), value.into());
        self
    }

    async fn save(self) {
        METRICS.lock().await.gauges.replace(self);
    }
}

impl Hash for Metric {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(self.name.as_bytes());
        for (k, v) in &self.labels {
            state.write(k.as_bytes());
            state.write(v.as_bytes());
        }
    }
}

impl PartialEq for Metric {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.labels == other.labels
    }
}

impl Eq for Metric {}

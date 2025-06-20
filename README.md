# prometheus-http-exporter

Turn HTTP resources into Prometheus metrics.

> [!IMPORTANT]  
> Excessive amounts of requests can lead to your being banned,
> and is generally regarded as a dick move. **Use it responsibly.**

* [Concepts](#concepts)
* [Quickstart](#quickstart)
* [Configuration](#configuration)
* [Extractors](#extractors)
    * [jq](#jq)
    * [Regex](#regex)
* [Prometheus Configuration](#prometheus-configuration)

## Concepts

The configuration file contains a list of **targets**.
Each target represents one URL (one foreign endpoint) that the exporter will scrape.

Each target contains a set of **rules**, which transform the URL's response into metrics.

Finally, the metrics are exposed on a configurable port, to be scraped by Prometheus.
See [here](#Prometheus-Configuration) for how to configure Prometheus.

> [!NOTE]
> Unlike most exporters, this project does not generate fresh metrics when scraped by Prometheus.
> Instead, each target keeps its own schedule, as defined in the config.
>
> This is to support both low- and high-frequency metrics in the same exporter.

## Quickstart

Save the following as `config.yml`:

```yaml
scrape_on_startup: true
targets:
  - name: prometheus repository stats
    url: https://api.github.com/repos/prometheus/prometheus
    cron: "* 0 * * * *"
    rules:
      - name: prometheus_repo_watchers
        extract: .watchers
      - name: prometheus_repo_stars
        extract: .stargazers_count
      - name: prometheus_repo_forks
        extract: .forks
```

Now run the exporter:

````bash
$ docker run -v "${PWD}/config.yml:/config.yml" -it ghcr.io/mcofficer/prometheus-http-exporter:latest
````

...and check [0.0.0.0:3000/metrics](http://0.0.0.0:3000/metrics):

```bash
$ curl http://0.0.0.0:3000/metrics
################### prometheus repository stats ###################

# TYPE prometheus_repo_watchers gauge
prometheus_repo_watchers 59046 1750334436432

# TYPE prometheus_repo_stars gauge
prometheus_repo_stars 59046 1750334436433

# TYPE prometheus_repo_forks gauge
prometheus_repo_forks 9601 1750334436433
```

## Configuration

```yaml
# The Address to bind to. Defaults to 0.0.0.0:3000
address: 0.0.0.0:8271
# Scrapes each target while starting up. Useful to test your config, don't use in production.
scrape_on_startup: true
# Log level, "info" by default
log_level: debug
targets:
  - name: crates.io summary
    url: https://crates.io/api/v1/summary
    cron: every 15 minutes
    extractor: jq # jq is the default, so this could be omitted
    headers:
      # crates.io requests that we identify ourselves & provide contact info
      User-Agent: "prometheus_http_exporter/0.1.0 (Hosted by John Doe <jd@example.org>)"
    rules:
      - name: crates_io_crates
        extract: ".num_crates"
```

Read on for an explanation of the most important parts, or see `config.yml` for a full example.
There is also an auto-generated json schema available (`config.schema.json`).

#### cron

Specifies when the job should run. Two formats are supported:

- english expressions, such as `every 15 minutes` or `every day`. See
  the [english-to-cron](https://github.com/kaplanelad/english-to-cron#full-list-of-supported-english-patterns) crate for
  a table of valid patterns.
- classic cron syntax, f.e. `* */15 * * * *` or `@daily`. The only caveat here is that the seconds field
  is [not optional](https://github.com/mvniekerk/tokio-cron-scheduler/issues/95).
  Refer to the [croner](https://github.com/Hexagon/croner-rust#pattern) documentation for specifics.

#### Extractor

See [Extractors](#Extractors)

#### Headers

Custom headers can be included with the `headers` map.
The exporter automatically sets the User-Agent, but you may override it here.

## Extractors

Extractors are the heart of the exporter, being responsible for turning a response into metrics.
The chosen extractor runs once for each rule, each time producing one (or several) metrics with the rule's name.
The `extract` key on each rule is used to configure the extractor.

Currently, two extractors are supported: jq (the default), and Regex.

### jq

[jq](https://jqlang.org) describes itself as "a lightweight and flexible command-line JSON processor."
In practice, its query language is Turing-complete and people have even
implemented [jq in jq](https://github.com/wader/jqjq).

This project uses the [jaq](https://github.com/01mf02/jaq), a rust clone of jq.
For 99% of cases, jaq and jq are interchangeable, and you can usually expect queries from
the [jq playground](https://play.jqlang.org/) to work with jaq.

#### Extracing single values

The simplest use-case for jq is to extract a single number from a JSON response.
For instance, let's look at the QuickStart example, which uses the GitHub API to get information about
the [prometheus](https://github.com/prometheus/prometheus) repository.

The GitHub API returns a response like this:

```json
{
  "full_name": "prometheus/prometheus",
  ...
  "stargazers_count": 59046
}
```

... from which the query `.stargazers_count` extracts the value: `59046`.
<sup>[(jq Playground)](https://play.jqlang.org/s/pYWD00NiK8bDNz4)</sup>

If the value is a number, a metric is emitted using the rule's name, the value and the timestamp of the extraction:

````
# TYPE prometheus_repo_stars gauge
prometheus_repo_stars 59046 1750334436433
````

#### Extracting multiple values

Sometimes, a response contains more than a few values of interest.
Rather than creating a bespoke rule for each, we can have a query that returns several at once.
Consider the following JSON response:

```json
{
  "yaks": {
    "shaved": 3,
    "total": 5
  }
}
```

Suppose we are interested in both `shaved` and `total`.
We can use the query `.yaks` to return only the object containing both.
<sup>[(jq Playground)](https://play.jqlang.org/s/0afwh6UpcWsjRB-)</sup>

The extractor will emit a metric for each key-value pair in the object (if the value is a number), with the key being
preserved as a label:

```
# TYPE yaks gauge
yaks{key="shaved"} 3 1750338779649
yaks{key="total"} 5 1750338779649
```

Similarly, the extractor can also accommodate arrays:

```json
[
  {
    "value": 3,
    "shaved": true
  },
  {
    "value": 2,
    "shaved": false
  }
]
```

In this case, each object containing a numeric `value` is turned into a metric.
The object's other contents (except nested objects and arrays) are attached as labels:

```
# TYPE yaks gauge
yaks{shaved="true"} 3 1750340568904
yaks{shaved="false"} 2 1750340568904
```

> [!WARNING]  
> Be careful when indiscriminately ingesting data.
> While every key/value
> is [strictly sanitized](https://prometheus.io/docs/instrumenting/escaping_schemes/#underscore-escaping-underscores),
> this does not protect you from large and unnecessary amounts of data.
> For example, one might end up with labels containing base64-encoded images.

### Regex

[Regular expressions](https://en.wikipedia.org/wiki/Regular_expression) have become a programming mainstay,
in part due to their flexibility to find things in virtually any human-readable input.
However, they can be all but inscrutable to beginners.
If you're struggling, try using a regex debugger such as [regex101](https://regex101.com/?flavor=rust&flags=) and
maybe have a look at the Learning section of [awesome-regex](https://github.com/aloisdg/awesome-regex#learning).

This project uses the [regex](https://github.com/rust-lang/regex) crate,
which omits some of the more compute-intensive regex features (most notably, look-arounds).
The "Rust" flavor on regex101 uses the same crate.
[By default](https://docs.rs/regex/latest/regex/struct.RegexBuilder.html), all Regex flags except Unicode support are disabled.

> [!NOTE]  
> Regex is far from a perfect format,
> and scraping content meant for humans is generally frowned upon by website operators.
> Regex support in this project is intended as a fallback feature -
> if at all possible, you should prefer an extractor that parses structured data.

#### Example

Let's try to extract the statistics from [Steam's About page](https://store.steampowered.com/about/). Their HTML
contains this snippet:

```html
<div class="online_stat_label gamers_online">online</div>
    36,426,658                            </div>
```

A naive regex might look something like this:
<sup>[(regex101)](https://regex101.com/r/z2XWsH/1)</sup>

```regexp
gamers_online.*?\s*?([\d,]+)
```

So let's plug that into a rule, enable `scrape_on_startup`, and-

```
called `Result::unwrap()` on an `Err` value: Regex matched, but the result could not be parsed as f64: '36,426,658'
```

Oh.

This is unfortunately a common problem with matching numbers made for humans - there are commas that the `f64`-parser
can't make sense of. It's not the parser's fault; formatting rules vary depending on locale, and making a "best guess"
is both difficult and unreliable.

Fortunately, there is a solution. If our regex has multiple matching capture groups, they will first be concatenated,
then parsed as number. (This does not apply to non-capturing groups). So let's adjust our regex:
<sup>[(regex101)](https://regex101.com/r/vFafER/1)</sup>

```regexp
gamers_online.*\s*(\d+),?(\d+),?(\d+)
```

It's ugly, but it gives us 3 groups of comma-separated segments.
(If Steam ever goes over a billion players, we'll need to add a fourth.)
This finally gives us results:

```
# TYPE steam_players_online gauge
steam_players_online 36426658 1750350123528
```

#### Rules

The Regex extractor follows these rules:

- Only the first match is processed, all others are discarded.
- If there are no matching capture groups, or no groups at all, parse the entire match.
- If there are only unnamed capture groups, concatenate and then parse them.
- If there are named capture groups, parse the group called `value` and add all others as labels.

## Prometheus Configuration

Add this to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: prometheus-http-exporter
    honor_timestamps: true
    scrape_interval: 5m
    static_configs:
      - targets: [ 0.0.0.0:8271 ]
```

**`honor_timestamps` is true by default and may be omitted.** It is not recommended to set it to false;
In that case Prometheus will give the metrics a fresh timestamp on every scrape,
even if the exporter hasn't updated some metric in hours.
It makes for better-looking dashboards, but at the cost of polluting Prometheus with misleading data.

`scrape_interval` may be set as low as possible, at least as low as the shortest scheduled target.

Remember to replace `0.0.0.0:8271` with the address defined in the prometheus-http-exporter config.

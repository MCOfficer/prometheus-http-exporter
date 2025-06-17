# prometheus-http-exporter

Turn HTTP resources into Prometheus metrics.

> [!IMPORTANT]  
> Excessive amounts of requests can lead to your being banned,
> and is generally regarded as a dick move. **Use it responsibly.**

## Concepts

The configuration file contains a list of **targets**.
Each target represents one URL (one foreign endpoint) that the exporter will scrape.

Each target contains a set of **rules**, which transform the URL's response into metrics.

Finally, the metrics are exposed on a configurable port, to be scraped by Prometheus.
See [#Prometheus Configuration](#Prometheus-Configuration) for how to configure Prometheus.

> [!NOTE]
> Unlike most exporters, this project does not generate fresh metrics when scraped by Prometheus.
> Instead, each target keeps its own schedule, as defined in the config.
>
> This is to support both low- and high-frequency metrics in the same exporter.

## Configuration

```yaml
# The Address to bind to. Defaults to 0.0.0.0:3000
address: 0.0.0.0:8271
# Scrapes each target while starting up. Useful to test your config, don't use in production.
scrape_on_startup: true
targets:
  - name: crates.io summary
    url: https://crates.io/api/v1/summary
    cron: every 15 minutes
    extractor: jq # default, may be omitted
    rules:
      - name: crates_io_crates
        extract: ".num_crates"
```
See `config.yml` for a full example.

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

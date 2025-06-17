# prometheus-http-exporter

Turn HTTP responses into prometheus metrics.

## Concepts

The exporter exposes a configurable port, from which prometheus can retrieve the final metrics.
See [#Prometheus Configuration](#Prometheus-Configuration) for how to configure Prometheus.

The configuration file contains a list of **targets**.
Each target represents on URL (one foreign endpoint) that the exporter will scrape.

Each target contains a set of **rules**, which transform the returned data into metrics.

> [!NOTE]
> Unlike most exporters, this project does not generate fresh metrics when scraped.
> Instead, each target keeps its own schedule, as defined in the config.
>
> This is to support both high-frequency scraping of internal and low-frequency scraping of external APIs,
> in the same exporter.

## Configuration

```yaml
# The Address to bind to. Defaults to 0.0.0.0:3000
address: 0.0.0.0:8271
# Scrapes each target while starting up. Useful to test your config, don't use in production.
scrape_on_startup: true
targets:
  - name: ISRO Spacecraft Count
    # original url: https://isro.vercel.app/api/spacecrafts
    url: https://gist.githubusercontent.com/MCOfficer/cfeb0111e652c1a332c861dcae68c024/raw/isro-spacecrafts.json
    cron: every day # uses
    rules:
      - name: isro_spacecraft_count
        extract: ".spacecrafts | length"
        metric_type: gauge
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
It makes for better-looking dashboards, but at the cost of polluting prometheus with misleading data.

`scrape_interval` may be set as low as possible, at least as low as the shortest scheduled target.

Remember to replace `0.0.0.0:8271` with the address defined in the prometheus-http-exporter config.

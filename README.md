[![Rust CI](https://github.com/RevianLabs/tplink-plug-exporter/actions/workflows/rust.yml/badge.svg)](https://github.com/RevianLabs/tplink-plug-exporter/actions/workflows/rust.yml)
[![Docker Image CI](https://github.com/RevianLabs/tplink-plug-exporter/actions/workflows/docker-image.yml/badge.svg)](https://github.com/RevianLabs/tplink-plug-exporter/actions/workflows/docker-image.yml)

# tplink-plug-exporter (Rust)

A Prometheus exporter for TP-Link Kasa Smart Plugs and Power Strips, rewritten in Rust for high performance, memory efficiency, and minimal container footprint.

## Features

- **Full TP-Link Smart Home protocol support**:
  - Autokey XOR cipher (initial key 171) over TCP port 9999
  - 4-byte big-endian length prefix framing
  - Legacy float units and newer milli-unit firmware normalization
- **Multi-plug / Power strip support** (e.g. HS107, KP303, KP400) via `context.child_ids`
- **Prometheus multi-target exporter pattern** via `/scrape?target=<plug-ip>`
- **Prometheus text exposition format** via official `prometheus-client` crate
- **Async networking** built on Tokio and Axum

## Metrics Exposed

| Metric Name | Type | Labels | Description |
|---|---|---|---|
| `kasa_online` | Gauge | none | Device online reachability (1 = reachable, 0 = unreachable) |
| `kasa_metadata` | Gauge | `alias`, `hw_ver`, `sw_ver`, `model`, `feature` | Constant 1 with device hardware and firmware metadata |
| `kasa_relay_state` | Gauge | `alias`, `id` | Switch state (1 = on, 0 = off) |
| `kasa_on_time` | Gauge | `alias`, `id` | Seconds elapsed since last state change |
| `kasa_rssi` | Gauge | `alias`, `id` | Wi-Fi received signal strength indicator (dBm) |
| `kasa_current` | Gauge | `alias`, `id` | Current load in Amperes (A) |
| `kasa_voltage` | Gauge | `alias`, `id` | Voltage in Volts (V) |
| `kasa_power_load` | Gauge | `alias`, `id` | Current instantaneous power consumption in Watts (W) |
| `kasa_power_total` | Gauge | `alias`, `id` | Total energy consumption in kilowatt-hours (kWh) |

## Quick Start

### Running with Docker

```bash
docker run -d -p 9233:9233 tplink-plug-exporter:latest
```

Custom listen address:
```bash
docker run -d -p 8080:8080 tplink-plug-exporter:latest --metrics.listen-addr :8080
```

### Building from Source

Prerequisites: Rust 1.80+ / Cargo

```bash
cargo build --release
./target/release/tplink-plug-exporter --metrics.listen-addr :9233
```

### Running Tests

The test suite includes unit tests, cipher verification, JSON schema handling, and end-to-end integration tests using in-memory mock TCP smart plug servers:

```bash
cargo test
```

## Prometheus Configuration Example

Configure Prometheus using the standard multi-target scraping pattern:

```yaml
scrape_configs:
  - job_name: 'kasa'
    scrape_interval: 15s
    static_configs:
      - targets:
        - 192.168.1.100  # IP of smart plug 1
        - 192.168.1.101  # IP of smart plug 2
    metrics_path: /scrape
    relabel_configs:
      - source_labels: [__address__]
        target_label: __param_target
      - source_labels: [__param_target]
        target_label: instance
      - target_label: __address__
        replacement: localhost:9233  # Address of this exporter

  - job_name: 'kasa_exporter'
    static_configs:
      - targets:
        - localhost:9233
```


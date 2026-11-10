use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

use crate::kasa::{KasaClient, KasaClientConfig, KasaRequestContext};

/// Label set for most per-plug metrics.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct PlugLabels {
    pub alias: String,
    pub id: String,
}

/// Label set for device metadata (info gauge).
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MetadataLabels {
    pub alias: String,
    pub hw_ver: String,
    pub sw_ver: String,
    pub model: String,
    pub feature: String,
}

/// Collect metrics from a TP-Link smart plug at `host` and return Prometheus
/// text-format output.
pub async fn collect_metrics(host: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = KasaClient::new(&KasaClientConfig {
        host: host.to_string(),
    });
    collect_metrics_from_client(&client).await
}

/// Collect metrics using an existing `KasaClient`.
pub async fn collect_metrics_from_client(
    client: &KasaClient,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut registry = Registry::default();

    // ── online gauge (no labels) ────────────────────────────────────────
    let online: Gauge<f64, AtomicU64> = Gauge::default();
    registry.register(
        "kasa_online",
        "Whether the plug is reachable",
        online.clone(),
    );

    // Attempt to reach the device
    let sysinfo = match client.get_sysinfo(None).await {
        Ok(info) => info,
        Err(_) => {
            online.set(0.0);
            let mut buf = String::new();
            encode(&mut buf, &registry)?;
            return Ok(buf);
        }
    };

    online.set(1.0);

    // ── metadata gauge ──────────────────────────────────────────────────
    let metadata: Family<MetadataLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register("kasa_metadata", "Device metadata", metadata.clone());

    metadata
        .get_or_create(&MetadataLabels {
            alias: sysinfo.alias.clone(),
            hw_ver: sysinfo.hw_ver.clone(),
            sw_ver: sysinfo.sw_ver.clone(),
            model: sysinfo.model.clone(),
            feature: sysinfo.feature.clone(),
        })
        .set(1.0);

    // ── per-plug gauges ─────────────────────────────────────────────────
    let relay_state: Family<PlugLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register(
        "kasa_relay_state",
        "Relay state (on/off)",
        relay_state.clone(),
    );

    let on_time: Family<PlugLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register(
        "kasa_on_time",
        "Seconds since last state change",
        on_time.clone(),
    );

    let rssi: Family<PlugLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register("kasa_rssi", "WiFi RSSI (dBm)", rssi.clone());

    let current: Family<PlugLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register("kasa_current", "Current (amps)", current.clone());

    let voltage: Family<PlugLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register("kasa_voltage", "Voltage (volts)", voltage.clone());

    let power_load: Family<PlugLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register("kasa_power_load", "Power load (watts)", power_load.clone());

    let power_total: Family<PlugLabels, Gauge<f64, AtomicU64>> = Family::default();
    registry.register(
        "kasa_power_total",
        "Total energy (kWh)",
        power_total.clone(),
    );

    // Helper to set the common per-plug gauges
    let set_plug_metrics = |labels: &PlugLabels, rs: i32, ot: i64, r: i32| {
        relay_state.get_or_create(labels).set(rs as f64);
        on_time.get_or_create(labels).set(ot as f64);
        rssi.get_or_create(labels).set(r as f64);
    };

    let parent_labels = PlugLabels {
        alias: sysinfo.alias.clone(),
        id: sysinfo.device_id.clone(),
    };

    // Set parent metrics
    set_plug_metrics(
        &parent_labels,
        sysinfo.relay_state,
        sysinfo.on_time,
        sysinfo.rssi,
    );

    // ── children (power strip) ──────────────────────────────────────────
    if !sysinfo.children.is_empty() {
        for child in &sysinfo.children {
            let child_labels = PlugLabels {
                alias: child.alias.clone(),
                id: child.id.clone(),
            };
            relay_state
                .get_or_create(&child_labels)
                .set(child.state as f64);
            on_time
                .get_or_create(&child_labels)
                .set(child.on_time as f64);
        }
    }

    // ── emeter (energy monitoring) ──────────────────────────────────────
    if KasaClient::emeter_supported(&sysinfo) {
        // Parent emeter
        if let Ok(mut realtime) = client.get_realtime(None).await {
            realtime.normalize();
            current.get_or_create(&parent_labels).set(realtime.current);
            voltage.get_or_create(&parent_labels).set(realtime.voltage);
            power_load.get_or_create(&parent_labels).set(realtime.power);
            power_total
                .get_or_create(&parent_labels)
                .set(realtime.total);
        }

        // Per-child emeter
        for child in &sysinfo.children {
            let ctx = KasaRequestContext {
                child_ids: vec![child.id.clone()],
            };
            if let Ok(mut realtime) = client.get_realtime(Some(&ctx)).await {
                realtime.normalize();
                let child_labels = PlugLabels {
                    alias: child.alias.clone(),
                    id: child.id.clone(),
                };
                current.get_or_create(&child_labels).set(realtime.current);
                voltage.get_or_create(&child_labels).set(realtime.voltage);
                power_load.get_or_create(&child_labels).set(realtime.power);
                power_total.get_or_create(&child_labels).set(realtime.total);
            }
        }
    }

    // ── encode ──────────────────────────────────────────────────────────
    let mut buf = String::new();
    encode(&mut buf, &registry)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_collect_metrics_unreachable_returns_online_zero() {
        // Point at a host that will not respond
        let result = collect_metrics("192.0.2.1").await;
        assert!(
            result.is_ok(),
            "collect_metrics should not error on unreachable host"
        );
        let text = result.unwrap();
        // Should contain kasa_online 0
        assert!(
            text.contains("kasa_online 0"),
            "Expected kasa_online 0 in output, got:\n{}",
            text
        );
    }

    #[tokio::test]
    async fn test_collect_metrics_output_is_valid_prometheus_text() {
        // Even for an unreachable host, the output should be valid prometheus text
        let text = collect_metrics("192.0.2.1").await.unwrap();
        // Prometheus text format lines are either comments (# ...) or metric lines
        for line in text.lines() {
            assert!(
                line.starts_with('#')
                    || line.starts_with("kasa_")
                    || line.is_empty()
                    || line.starts_with("# EOF"),
                "Unexpected line in prometheus output: {}",
                line
            );
        }
    }

    #[test]
    fn test_plug_labels_equality() {
        let a = PlugLabels {
            alias: "plug1".into(),
            id: "abc".into(),
        };
        let b = PlugLabels {
            alias: "plug1".into(),
            id: "abc".into(),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn test_metadata_labels_equality() {
        let a = MetadataLabels {
            alias: "plug".into(),
            hw_ver: "1.0".into(),
            sw_ver: "2.0".into(),
            model: "HS110".into(),
            feature: "TIM:ENE".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn test_collect_metrics_full_success_with_mock() {
        use crate::kasa::{decrypt, encrypt, pack_int, unpack_int};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Spawn mock server that responds to requests
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let mut len_buf = [0u8; 4];
                if socket.read_exact(&mut len_buf).await.is_err() {
                    break;
                }
                let msg_len = unpack_int(&len_buf) as usize;
                let mut req_buf = vec![0u8; msg_len];
                if socket.read_exact(&mut req_buf).await.is_err() {
                    break;
                }
                let req_decrypted = decrypt(&req_buf);
                let req_json: serde_json::Value = serde_json::from_slice(&req_decrypted).unwrap();

                let resp_json = if req_json.get("system").is_some() {
                    serde_json::json!({
                        "system": {
                            "get_sysinfo": {
                                "err_code": 0,
                                "mac": "50:C7:BF:00:11:22",
                                "model": "KP115(US)",
                                "alias": "Living Room Lamp",
                                "feature": "TIM:ENE",
                                "relay_state": 1,
                                "rssi": -55,
                                "led_off": 0,
                                "on_time": 7200,
                                "deviceId": "DEV123456",
                                "sw_ver": "1.0.10",
                                "hw_ver": "1.0",
                                "children": [
                                    {
                                        "id": "DEV123456_01",
                                        "alias": "Socket 1",
                                        "state": 1,
                                        "on_time": 3600
                                    }
                                ]
                            }
                        }
                    })
                } else if req_json.get("emeter").is_some() {
                    serde_json::json!({
                        "emeter": {
                            "get_realtime": {
                                "err_code": 0,
                                "current_ma": 450.0,
                                "voltage_mv": 121500.0,
                                "power_mw": 54675.0,
                                "total_wh": 10500.0
                            }
                        }
                    })
                } else {
                    serde_json::json!({"err_code": -1})
                };

                let resp_bytes = serde_json::to_vec(&resp_json).unwrap();
                let enc_bytes = encrypt(&resp_bytes);
                let prefix = pack_int(enc_bytes.len() as u32);

                let _ = socket.write_all(&prefix).await;
                let _ = socket.write_all(&enc_bytes).await;
                let _ = socket.flush().await;
            }
        });

        let target = format!("127.0.0.1:{port}");
        let text = collect_metrics(&target).await.unwrap();

        assert!(text.contains("kasa_online 1"), "Must be online");
        assert!(text.contains("kasa_metadata"), "Must contain metadata");
        assert!(text.contains("KP115(US)"), "Must contain model");
        assert!(
            text.contains("kasa_relay_state"),
            "Must contain relay state"
        );
        assert!(text.contains("kasa_power_load"), "Must contain power load");
        assert!(
            text.contains("kasa_power_total"),
            "Must contain power total"
        );
        assert!(text.contains("kasa_current"), "Must contain current");
        assert!(text.contains("kasa_voltage"), "Must contain voltage");
        assert!(text.contains("Socket 1"), "Must contain child plug metrics");
    }

    #[tokio::test]
    async fn test_collect_metrics_device_without_ene() {
        use crate::kasa::{encrypt, pack_int, unpack_int};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            let _ = socket.read_exact(&mut len_buf).await;
            let msg_len = unpack_int(&len_buf) as usize;
            let mut req_buf = vec![0u8; msg_len];
            let _ = socket.read_exact(&mut req_buf).await;

            let resp_json = serde_json::json!({
                "system": {
                    "get_sysinfo": {
                        "err_code": 0,
                        "mac": "AA:BB:CC:DD:EE:FF",
                        "model": "HS100(US)",
                        "alias": "Simple Switch",
                        "feature": "TIM", // Note: NO "ENE"
                        "relay_state": 1,
                        "rssi": -48,
                        "led_off": 0,
                        "on_time": 600,
                        "deviceId": "HS100_DEV",
                        "sw_ver": "1.1.0",
                        "hw_ver": "1.0",
                        "children": []
                    }
                }
            });

            let resp_bytes = serde_json::to_vec(&resp_json).unwrap();
            let enc_bytes = encrypt(&resp_bytes);
            let prefix = pack_int(enc_bytes.len() as u32);
            let _ = socket.write_all(&prefix).await;
            let _ = socket.write_all(&enc_bytes).await;
        });

        let target = format!("127.0.0.1:{port}");
        let text = collect_metrics(&target).await.unwrap();

        assert!(text.contains("kasa_online 1"));
        assert!(text.contains("Simple Switch"));
        assert!(text.contains("HS100(US)"));
        assert!(text.contains("kasa_relay_state{"));
        // Since feature is TIM without ENE, energy metric samples should not exist
        assert!(!text.contains("kasa_power_load{"));
        assert!(!text.contains("kasa_power_total{"));
        assert!(!text.contains("kasa_current{"));
        assert!(!text.contains("kasa_voltage{"));
    }

    #[tokio::test]
    async fn test_collect_metrics_emeter_failure_retains_online() {
        use crate::kasa::{encrypt, pack_int, unpack_int};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let mut is_first = true;
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let mut len_buf = [0u8; 4];
                if socket.read_exact(&mut len_buf).await.is_err() {
                    break;
                }
                let msg_len = unpack_int(&len_buf) as usize;
                let mut req_buf = vec![0u8; msg_len];
                if socket.read_exact(&mut req_buf).await.is_err() {
                    break;
                }

                let resp_json = if is_first {
                    is_first = false;
                    serde_json::json!({
                        "system": {
                            "get_sysinfo": {
                                "err_code": 0,
                                "mac": "AA:BB:CC:11:22:33",
                                "model": "KP115(EU)",
                                "alias": "Plug With Glitched Emeter",
                                "feature": "TIM:ENE",
                                "relay_state": 1,
                                "rssi": -50,
                                "led_off": 0,
                                "on_time": 100,
                                "deviceId": "KP115_DEV",
                                "sw_ver": "1.0.0",
                                "hw_ver": "1.0",
                                "children": []
                            }
                        }
                    })
                } else {
                    // Emeter fails with error code
                    serde_json::json!({
                        "emeter": {
                            "get_realtime": {
                                "err_code": -1
                            }
                        }
                    })
                };

                let resp_bytes = serde_json::to_vec(&resp_json).unwrap();
                let enc_bytes = encrypt(&resp_bytes);
                let prefix = pack_int(enc_bytes.len() as u32);
                let _ = socket.write_all(&prefix).await;
                let _ = socket.write_all(&enc_bytes).await;
            }
        });

        let target = format!("127.0.0.1:{port}");
        let text = collect_metrics(&target).await.unwrap();

        // Device was reachable, so online should be 1 and metadata present
        assert!(text.contains("kasa_online 1"));
        assert!(text.contains("Plug With Glitched Emeter"));
        assert!(text.contains("kasa_relay_state"));
    }
}

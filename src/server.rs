use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::exporter;

#[derive(serde::Deserialize)]
pub struct ScrapeParams {
    pub target: String,
}

/// Handler for `GET /scrape?target=<host>`.
/// Collects metrics from the given TP-Link plug and returns Prometheus text.
pub async fn scrape_handler(Query(params): Query<ScrapeParams>) -> impl IntoResponse {
    match exporter::collect_metrics(&params.target).await {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
            body,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error collecting metrics: {e}"),
        )
            .into_response(),
    }
}

/// Handler for `GET /metrics`. Returns placeholder process metrics.
pub async fn metrics_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        "# tplink-plug-exporter\n",
    )
}

/// Build the axum router with `/scrape` and `/metrics` routes.
pub fn create_router() -> Router {
    Router::new()
        .route("/scrape", get(scrape_handler))
        .route("/metrics", get(metrics_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for oneshot

    #[tokio::test]
    async fn test_metrics_handler_returns_200() {
        let app = create_router();
        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_scrape_missing_target_returns_400() {
        let app = create_router();
        // No query parameter → axum returns 400 for missing required query params
        let req = Request::builder()
            .uri("/scrape")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_scrape_unreachable_target_returns_200_with_online_zero() {
        let app = create_router();
        let req = Request::builder()
            .uri("/scrape?target=192.0.2.1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("kasa_online 0"),
            "Expected kasa_online 0, got:\n{}",
            text
        );
    }

    #[tokio::test]
    async fn test_router_has_scrape_route() {
        let app = create_router();
        let req = Request::builder()
            .uri("/scrape?target=192.0.2.1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Should not be 404
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_router_has_metrics_route() {
        let app = create_router();
        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_scrape_successful_target_returns_200_with_metrics() {
        use crate::kasa::{decrypt, encrypt, pack_int, unpack_int};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Spawn mock device
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
                                "mac": "AA:BB:CC:DD:EE:00",
                                "model": "HS110(US)",
                                "alias": "Heater Plug",
                                "feature": "TIM:ENE",
                                "relay_state": 1,
                                "rssi": -60,
                                "led_off": 0,
                                "on_time": 1800,
                                "deviceId": "PLUG999",
                                "sw_ver": "1.5.0",
                                "hw_ver": "1.0",
                                "children": []
                            }
                        }
                    })
                } else {
                    serde_json::json!({
                        "emeter": {
                            "get_realtime": {
                                "err_code": 0,
                                "current_ma": 8500.0,
                                "voltage_mv": 120500.0,
                                "power_mw": 1024250.0,
                                "total_wh": 50000.0
                            }
                        }
                    })
                };

                let resp_bytes = serde_json::to_vec(&resp_json).unwrap();
                let enc_bytes = encrypt(&resp_bytes);
                let prefix = pack_int(enc_bytes.len() as u32);

                let _ = socket.write_all(&prefix).await;
                let _ = socket.write_all(&enc_bytes).await;
                let _ = socket.flush().await;
            }
        });

        let app = create_router();
        let uri = format!("/scrape?target=127.0.0.1:{port}");
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("kasa_online 1"));
        assert!(text.contains("Heater Plug"));
        assert!(text.contains("PLUG999"));
        assert!(text.contains("kasa_power_load"));
    }

    #[tokio::test]
    async fn test_metrics_content_type_header() {
        let app = create_router();
        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/plain"),
            "Content-type should be text/plain"
        );
    }

    #[tokio::test]
    async fn test_post_to_metrics_returns_method_not_allowed() {
        let app = create_router();
        let req = Request::builder()
            .method("POST")
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_scrape_with_extra_query_params() {
        let app = create_router();
        // Extra query params shouldn't cause errors as long as `target` is provided
        let req = Request::builder()
            .uri("/scrape?target=192.0.2.1&debug=1&extra=value")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

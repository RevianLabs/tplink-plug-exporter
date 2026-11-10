use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const INITIAL_KEY: u8 = 171;
const TIMEOUT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum KasaError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("timeout: {0}")]
    Timeout(#[from] tokio::time::error::Elapsed),

    #[error("RPC error: service={service} cmd={cmd} err_code={err_code}")]
    Rpc {
        service: String,
        cmd: String,
        err_code: i64,
    },

    #[error("unexpected response structure")]
    UnexpectedResponse,
}

pub type Result<T> = std::result::Result<T, KasaError>;

// ---------------------------------------------------------------------------
// Cipher helpers
// ---------------------------------------------------------------------------

/// Encrypt data using the TP-Link autokey XOR cipher.
/// Key starts at 171; each *output* (ciphertext) byte becomes the next key.
pub fn encrypt(data: &[u8]) -> Vec<u8> {
    let mut key = INITIAL_KEY;
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        let cipher = b ^ key;
        key = cipher;
        out.push(cipher);
    }
    out
}

/// Decrypt data using the TP-Link autokey XOR cipher.
/// Key starts at 171; each *input* (ciphertext) byte becomes the next key.
pub fn decrypt(data: &[u8]) -> Vec<u8> {
    let mut key = INITIAL_KEY;
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        let plain = b ^ key;
        key = b;
        out.push(plain);
    }
    out
}

/// Pack a u32 into 4-byte big-endian bytes.
pub fn pack_int(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// Unpack 4 big-endian bytes into a u32.
pub fn unpack_int(buf: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*buf)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub struct KasaClient {
    addr: String,
}

pub struct KasaClientConfig {
    pub host: String,
}

pub struct KasaRequestContext {
    pub child_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GetSysInfoResponse {
    #[serde(default)]
    pub mac: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub feature: String,
    #[serde(default)]
    pub relay_state: i32,
    #[serde(default)]
    pub rssi: i32,
    #[serde(default)]
    pub led_off: i32,
    #[serde(default)]
    pub on_time: i64,
    #[serde(default, alias = "deviceId")]
    pub device_id: String,
    #[serde(default)]
    pub sw_ver: String,
    #[serde(default)]
    pub hw_ver: String,
    #[serde(default)]
    pub children: Vec<SysInfoChildren>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SysInfoChildren {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub state: i32,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub on_time: i64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GetRealtimeResponse {
    #[serde(default = "default_neg1")]
    pub current: f64,
    #[serde(default = "default_neg1")]
    pub voltage: f64,
    #[serde(default = "default_neg1")]
    pub power: f64,
    #[serde(default = "default_neg1")]
    pub total: f64,
    #[serde(default = "default_neg1")]
    pub current_ma: f64,
    #[serde(default = "default_neg1")]
    pub voltage_mv: f64,
    #[serde(default = "default_neg1")]
    pub power_mw: f64,
    #[serde(default = "default_neg1")]
    pub total_wh: f64,
}

fn default_neg1() -> f64 {
    -1.0
}

impl GetRealtimeResponse {
    /// If the firmware reports milli-unit fields (total_wh != -1), convert
    /// them to base units, overwriting the base-unit fields.
    pub fn normalize(&mut self) {
        if self.total_wh != -1.0 {
            self.current = self.current_ma / 1000.0;
            self.voltage = self.voltage_mv / 1000.0;
            self.power = self.power_mw / 1000.0;
            self.total = self.total_wh / 1000.0;
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

impl KasaClient {
    pub fn new(config: &KasaClientConfig) -> Self {
        let addr = if config.host.contains(':') {
            config.host.clone()
        } else {
            format!("{}:9999", config.host)
        };
        Self { addr }
    }

    /// Send a raw JSON payload over TCP using the Kasa protocol.
    /// Returns the decrypted response bytes.
    pub async fn request(&self, payload: &Value) -> Result<Vec<u8>> {
        let json_bytes = serde_json::to_vec(payload)?;
        let encrypted = encrypt(&json_bytes);

        // Build length-prefixed message
        let length_prefix = pack_int(encrypted.len() as u32);
        let mut message = Vec::with_capacity(4 + encrypted.len());
        message.extend_from_slice(&length_prefix);
        message.extend_from_slice(&encrypted);

        // Connect with timeout
        let mut stream = timeout(TIMEOUT, TcpStream::connect(&self.addr)).await??;

        // Write with timeout
        timeout(TIMEOUT, stream.write_all(&message)).await??;

        // Read 4-byte length header
        let mut len_buf = [0u8; 4];
        timeout(TIMEOUT, stream.read_exact(&mut len_buf)).await??;
        let resp_len = unpack_int(&len_buf) as usize;

        // Read response body
        let mut resp_buf = vec![0u8; resp_len];
        timeout(TIMEOUT, stream.read_exact(&mut resp_buf)).await??;

        Ok(decrypt(&resp_buf))
    }

    /// High-level RPC call. Builds the nested JSON envelope, optionally adds
    /// a request context with child_ids, sends the request, and parses the
    /// response. Returns the inner result object for the given service/cmd.
    pub async fn rpc(
        &self,
        service: &str,
        cmd: &str,
        ctx: Option<&KasaRequestContext>,
        payload: Value,
    ) -> Result<Value> {
        let mut body = serde_json::Map::new();
        body.insert(service.to_string(), serde_json::json!({ cmd: payload }));

        if let Some(context) = ctx {
            let child_ids: Vec<Value> = context
                .child_ids
                .iter()
                .map(|id| Value::String(id.clone()))
                .collect();
            body.insert(
                "context".to_string(),
                serde_json::json!({ "child_ids": child_ids }),
            );
        }

        let request = Value::Object(body);
        let response_bytes = self.request(&request).await?;
        let response: Value = serde_json::from_slice(&response_bytes)?;

        // Extract inner result: response[service][cmd]
        let service_obj = response.get(service).ok_or(KasaError::UnexpectedResponse)?;
        let result = service_obj.get(cmd).ok_or(KasaError::UnexpectedResponse)?;

        // Check err_code
        if let Some(err_code) = result.get("err_code").and_then(|v| v.as_i64()) {
            if err_code != 0 {
                return Err(KasaError::Rpc {
                    service: service.to_string(),
                    cmd: cmd.to_string(),
                    err_code,
                });
            }
        }

        Ok(result.clone())
    }

    /// Get system info from the device.
    pub async fn get_sysinfo(
        &self,
        ctx: Option<&KasaRequestContext>,
    ) -> Result<GetSysInfoResponse> {
        let result = self
            .rpc("system", "get_sysinfo", ctx, serde_json::json!({}))
            .await?;
        let info: GetSysInfoResponse = serde_json::from_value(result)?;
        Ok(info)
    }

    /// Get realtime emeter data from the device, with normalization.
    pub async fn get_realtime(
        &self,
        ctx: Option<&KasaRequestContext>,
    ) -> Result<GetRealtimeResponse> {
        let result = self
            .rpc("emeter", "get_realtime", ctx, serde_json::json!({}))
            .await?;
        let mut realtime: GetRealtimeResponse = serde_json::from_value(result)?;
        realtime.normalize();
        Ok(realtime)
    }

    /// Check whether the device supports emeter by looking for "ENE" in the
    /// feature string.
    pub fn emeter_supported(info: &GetSysInfoResponse) -> bool {
        info.feature.contains("ENE")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // 1. Encrypt / decrypt roundtrip
    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let original = b"hello world, this is a test payload!";
        let encrypted = encrypt(original);
        let decrypted = decrypt(&encrypted);
        assert_eq!(decrypted, original);
    }

    // 2. Encrypt with known values
    #[test]
    fn test_encrypt_known_values() {
        let input = b"hello";
        let encrypted = encrypt(input);
        // Manual calculation: key=171
        // 'h' (0x68) ^ 171 (0xAB) = 0xC3  -> key=0xC3
        // 'e' (0x65) ^ 0xC3 = 0xA6         -> key=0xA6
        // 'l' (0x6C) ^ 0xA6 = 0xCA         -> key=0xCA
        // 'l' (0x6C) ^ 0xCA = 0xA6         -> key=0xA6
        // 'o' (0x6F) ^ 0xA6 = 0xC9         -> key=0xC9
        assert_eq!(encrypted, vec![0xC3, 0xA6, 0xCA, 0xA6, 0xC9]);
    }

    // 3. Decrypt with known values
    #[test]
    fn test_decrypt_known_values() {
        let ciphertext = vec![0xC3, 0xA6, 0xCA, 0xA6, 0xC9];
        let decrypted = decrypt(&ciphertext);
        assert_eq!(decrypted, b"hello");
    }

    // 4. Pack / unpack roundtrip
    #[test]
    fn test_pack_unpack_roundtrip() {
        for &val in &[0u32, 1, 255, 256, 65535, 0x12345678, u32::MAX] {
            let packed = pack_int(val);
            let unpacked = unpack_int(&packed);
            assert_eq!(unpacked, val, "pack/unpack mismatch for {val}");
        }
    }

    // 5. RPC request JSON structure
    #[tokio::test]
    async fn test_rpc_json_structure() {
        // We'll build the JSON the same way rpc() does internally and verify structure.
        let service = "system";
        let cmd = "get_sysinfo";
        let payload = serde_json::json!({});

        let ctx = KasaRequestContext {
            child_ids: vec!["child1".to_string(), "child2".to_string()],
        };

        let mut body = serde_json::Map::new();
        body.insert(service.to_string(), serde_json::json!({ cmd: payload }));
        body.insert(
            "context".to_string(),
            serde_json::json!({ "child_ids": ctx.child_ids }),
        );
        let request = Value::Object(body);

        // Verify top-level key
        assert!(request.get("system").is_some());
        let sys = request.get("system").unwrap();
        // Verify cmd key
        assert!(sys.get("get_sysinfo").is_some());
        // Verify context is top-level sibling
        let context = request.get("context").unwrap();
        let ids = context.get("child_ids").unwrap().as_array().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].as_str().unwrap(), "child1");
        assert_eq!(ids[1].as_str().unwrap(), "child2");
    }

    // 5b. RPC request JSON structure without context
    #[tokio::test]
    async fn test_rpc_json_structure_no_context() {
        let service = "emeter";
        let cmd = "get_realtime";
        let payload = serde_json::json!({});

        let inner = serde_json::json!({ cmd: payload });
        let request = serde_json::json!({ service: inner });

        assert!(request.get("emeter").is_some());
        let eme = request.get("emeter").unwrap();
        assert!(eme.get("get_realtime").is_some());
        assert!(eme.get("context").is_none());
    }

    // 6. Normalize with milli-unit values
    #[test]
    fn test_normalize_milli_units() {
        let mut resp = GetRealtimeResponse {
            current: -1.0,
            voltage: -1.0,
            power: -1.0,
            total: -1.0,
            current_ma: 1500.0,
            voltage_mv: 120000.0,
            power_mw: 75000.0,
            total_wh: 12345.0,
        };
        resp.normalize();
        assert!((resp.current - 1.5).abs() < 1e-9);
        assert!((resp.voltage - 120.0).abs() < 1e-9);
        assert!((resp.power - 75.0).abs() < 1e-9);
        assert!((resp.total - 12.345).abs() < 1e-9);
    }

    // 7. Normalize no-op when total_wh is -1
    #[test]
    fn test_normalize_noop() {
        let mut resp = GetRealtimeResponse {
            current: 1.5,
            voltage: 120.0,
            power: 75.0,
            total: 12.345,
            current_ma: -1.0,
            voltage_mv: -1.0,
            power_mw: -1.0,
            total_wh: -1.0,
        };
        resp.normalize();
        assert!((resp.current - 1.5).abs() < 1e-9);
        assert!((resp.voltage - 120.0).abs() < 1e-9);
        assert!((resp.power - 75.0).abs() < 1e-9);
        assert!((resp.total - 12.345).abs() < 1e-9);
    }

    // 8. emeter_supported
    #[test]
    fn test_emeter_supported_with_ene() {
        let info = GetSysInfoResponse {
            mac: String::new(),
            model: String::new(),
            alias: String::new(),
            feature: "TIM:ENE".to_string(),
            relay_state: 0,
            rssi: 0,
            led_off: 0,
            on_time: 0,
            device_id: String::new(),
            sw_ver: String::new(),
            hw_ver: String::new(),
            children: vec![],
        };
        assert!(KasaClient::emeter_supported(&info));
    }

    #[test]
    fn test_emeter_supported_without_ene() {
        let info = GetSysInfoResponse {
            mac: String::new(),
            model: String::new(),
            alias: String::new(),
            feature: "TIM".to_string(),
            relay_state: 0,
            rssi: 0,
            led_off: 0,
            on_time: 0,
            device_id: String::new(),
            sw_ver: String::new(),
            hw_ver: String::new(),
            children: vec![],
        };
        assert!(!KasaClient::emeter_supported(&info));
    }

    // 9. Integration test with mock TCP server
    #[tokio::test]
    async fn test_mock_tcp_server_sysinfo() {
        // Start a mock server that speaks the Kasa protocol
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Read 4-byte length header
            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).await.unwrap();
            let msg_len = unpack_int(&len_buf) as usize;

            // Read encrypted request
            let mut req_buf = vec![0u8; msg_len];
            socket.read_exact(&mut req_buf).await.unwrap();
            let _request = decrypt(&req_buf);

            // Build a sysinfo response
            let response = serde_json::json!({
                "system": {
                    "get_sysinfo": {
                        "err_code": 0,
                        "mac": "AA:BB:CC:DD:EE:FF",
                        "model": "HS110(US)",
                        "alias": "Test Plug",
                        "feature": "TIM:ENE",
                        "relay_state": 1,
                        "rssi": -40,
                        "led_off": 0,
                        "on_time": 3600,
                        "deviceId": "ABCDEF1234567890",
                        "sw_ver": "1.0.0",
                        "hw_ver": "2.0",
                        "children": []
                    }
                }
            });

            let resp_bytes = serde_json::to_vec(&response).unwrap();
            let encrypted_resp = encrypt(&resp_bytes);
            let len_prefix = pack_int(encrypted_resp.len() as u32);

            socket.write_all(&len_prefix).await.unwrap();
            socket.write_all(&encrypted_resp).await.unwrap();
            socket.flush().await.unwrap();
        });

        let _config = KasaClientConfig {
            host: format!("127.0.0.1:{port}"),
        };
        // Use client with direct custom port
        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };

        let info = client.get_sysinfo(None).await.unwrap();
        assert_eq!(info.mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(info.model, "HS110(US)");
        assert_eq!(info.alias, "Test Plug");
        assert_eq!(info.feature, "TIM:ENE");
        assert_eq!(info.relay_state, 1);
        assert_eq!(info.rssi, -40);
        assert_eq!(info.on_time, 3600);
        assert_eq!(info.device_id, "ABCDEF1234567890");
        assert_eq!(info.sw_ver, "1.0.0");
        assert_eq!(info.hw_ver, "2.0");
        assert!(KasaClient::emeter_supported(&info));

        server.await.unwrap();
    }

    // 9b. Mock TCP server for emeter
    #[tokio::test]
    async fn test_mock_tcp_server_emeter() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).await.unwrap();
            let msg_len = unpack_int(&len_buf) as usize;

            let mut req_buf = vec![0u8; msg_len];
            socket.read_exact(&mut req_buf).await.unwrap();

            let response = serde_json::json!({
                "emeter": {
                    "get_realtime": {
                        "err_code": 0,
                        "current_ma": 1500.0,
                        "voltage_mv": 120000.0,
                        "power_mw": 75000.0,
                        "total_wh": 12345.0
                    }
                }
            });

            let resp_bytes = serde_json::to_vec(&response).unwrap();
            let encrypted_resp = encrypt(&resp_bytes);
            let len_prefix = pack_int(encrypted_resp.len() as u32);

            socket.write_all(&len_prefix).await.unwrap();
            socket.write_all(&encrypted_resp).await.unwrap();
            socket.flush().await.unwrap();
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };

        let realtime = client.get_realtime(None).await.unwrap();
        // Should be normalized from milli-units
        assert!((realtime.current - 1.5).abs() < 1e-9);
        assert!((realtime.voltage - 120.0).abs() < 1e-9);
        assert!((realtime.power - 75.0).abs() < 1e-9);
        assert!((realtime.total - 12.345).abs() < 1e-9);

        server.await.unwrap();
    }

    // 10. Connection failure
    #[tokio::test]
    async fn test_connection_failure() {
        let client = KasaClient {
            addr: "127.0.0.1:1".to_string(), // port 1 should refuse/timeout
        };
        let result = client.request(&serde_json::json!({"test": "hello"})).await;
        assert!(result.is_err());
    }

    // 10b. RPC error code handling
    #[tokio::test]
    async fn test_rpc_error_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).await.unwrap();
            let msg_len = unpack_int(&len_buf) as usize;
            let mut req_buf = vec![0u8; msg_len];
            socket.read_exact(&mut req_buf).await.unwrap();

            let response = serde_json::json!({
                "system": {
                    "get_sysinfo": {
                        "err_code": -1
                    }
                }
            });

            let resp_bytes = serde_json::to_vec(&response).unwrap();
            let encrypted_resp = encrypt(&resp_bytes);
            let len_prefix = pack_int(encrypted_resp.len() as u32);

            socket.write_all(&len_prefix).await.unwrap();
            socket.write_all(&encrypted_resp).await.unwrap();
            socket.flush().await.unwrap();
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };

        let result = client
            .rpc("system", "get_sysinfo", None, serde_json::json!({}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("err_code=-1"),
            "error should contain err_code: {err}"
        );

        server.await.unwrap();
    }

    // 11. Older firmware float-based emeter deserialization
    #[test]
    fn test_deserialize_older_firmware_emeter() {
        let json_str = r#"{
            "current": 0.414566,
            "voltage": 230.421503,
            "power": 49.65685,
            "total": 2.304,
            "err_code": 0
        }"#;

        let mut resp: GetRealtimeResponse = serde_json::from_str(json_str).unwrap();
        resp.normalize();
        assert_eq!(resp.total_wh, -1.0);
        assert!((resp.current - 0.414566).abs() < 1e-6);
        assert!((resp.voltage - 230.421503).abs() < 1e-6);
        assert!((resp.power - 49.65685).abs() < 1e-6);
        assert!((resp.total - 2.304).abs() < 1e-6);
    }

    // 12. SysInfo deserialization with children
    #[test]
    fn test_deserialize_sysinfo_with_children() {
        let json_str = r#"{
            "sw_ver": "1.0.4 Build 201111 Rel.145127",
            "hw_ver": "1.0",
            "model": "KP303(US)",
            "deviceId": "8006E881234567890",
            "alias": "3-Outlet Power Strip",
            "feature": "TIM:ENE",
            "mac": "50:D4:F7:11:22:33",
            "relay_state": 1,
            "rssi": -45,
            "children": [
                {
                    "id": "8006E88123456789000",
                    "state": 1,
                    "alias": "Plug 1",
                    "on_time": 100
                },
                {
                    "id": "8006E88123456789001",
                    "state": 0,
                    "alias": "Plug 2",
                    "on_time": 0
                }
            ]
        }"#;

        let info: GetSysInfoResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(info.model, "KP303(US)");
        assert_eq!(info.device_id, "8006E881234567890");
        assert_eq!(info.children.len(), 2);
        assert_eq!(info.children[0].alias, "Plug 1");
        assert_eq!(info.children[0].state, 1);
        assert_eq!(info.children[1].alias, "Plug 2");
        assert_eq!(info.children[1].state, 0);
    }

    // 13. KasaError formatting
    #[test]
    fn test_kasa_error_formatting() {
        let err = KasaError::Rpc {
            service: "system".to_string(),
            cmd: "reboot".to_string(),
            err_code: -100,
        };
        assert_eq!(
            err.to_string(),
            "RPC error: service=system cmd=reboot err_code=-100"
        );

        let err = KasaError::UnexpectedResponse;
        assert_eq!(err.to_string(), "unexpected response structure");
    }

    // 14. Encrypt/decrypt empty payload
    #[test]
    fn test_encrypt_decrypt_empty() {
        let empty: &[u8] = b"";
        let encrypted = encrypt(empty);
        assert!(encrypted.is_empty());
        let decrypted = decrypt(&encrypted);
        assert!(decrypted.is_empty());
    }

    // 15. Encrypt/decrypt large payload
    #[test]
    fn test_encrypt_decrypt_large_payload() {
        let large = vec![0x42u8; 16384];
        let encrypted = encrypt(&large);
        assert_eq!(encrypted.len(), large.len());
        let decrypted = decrypt(&encrypted);
        assert_eq!(decrypted, large);
    }

    // 16. KasaClient::new host format handling
    #[test]
    fn test_client_new_host_variations() {
        let c1 = KasaClient::new(&KasaClientConfig {
            host: "192.168.1.50".to_string(),
        });
        assert_eq!(c1.addr, "192.168.1.50:9999");

        let c2 = KasaClient::new(&KasaClientConfig {
            host: "192.168.1.50:12345".to_string(),
        });
        assert_eq!(c2.addr, "192.168.1.50:12345");
    }

    // 17. Server sends short header then EOF
    #[tokio::test]
    async fn test_mock_tcp_server_short_header_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 100];
            let _ = socket.read(&mut buf).await;
            // Write only 2 bytes instead of 4, then close
            let _ = socket.write_all(&[0, 1]).await;
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };
        let res = client.request(&serde_json::json!({})).await;
        assert!(
            res.is_err(),
            "Must error on premature EOF during header read"
        );
    }

    // 18. Server sends length header then premature EOF
    #[tokio::test]
    async fn test_mock_tcp_server_short_body_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 100];
            let _ = socket.read(&mut buf).await;
            // Send length prefix saying 50 bytes, but send only 5 bytes
            let _ = socket.write_all(&pack_int(50)).await;
            let _ = socket.write_all(&[1, 2, 3, 4, 5]).await;
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };
        let res = client.request(&serde_json::json!({})).await;
        assert!(res.is_err(), "Must error on premature EOF during body read");
    }

    // 19. Server sends response missing requested service or command
    #[tokio::test]
    async fn test_mock_tcp_server_missing_service_in_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            let _ = socket.read_exact(&mut len_buf).await;
            let msg_len = unpack_int(&len_buf) as usize;
            let mut req_buf = vec![0u8; msg_len];
            let _ = socket.read_exact(&mut req_buf).await;

            // Send unexpected JSON structure
            let resp = serde_json::json!({ "other_service": {} });
            let enc = encrypt(&serde_json::to_vec(&resp).unwrap());
            let _ = socket.write_all(&pack_int(enc.len() as u32)).await;
            let _ = socket.write_all(&enc).await;
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };
        let res = client
            .rpc("system", "get_sysinfo", None, serde_json::json!({}))
            .await;
        match res {
            Err(KasaError::UnexpectedResponse) => {}
            other => panic!("Expected UnexpectedResponse error, got {:?}", other),
        }
    }

    // 20. SysInfo deserialization with extra unknown fields
    #[test]
    fn test_deserialize_sysinfo_with_extra_fields() {
        let json_str = r#"{
            "sw_ver": "2.0.0",
            "hw_ver": "1.0",
            "model": "HS100(US)",
            "deviceId": "DEV111",
            "alias": "Lamp",
            "extra_future_field": 12345,
            "another_field": "hello"
        }"#;

        let info: GetSysInfoResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(info.model, "HS100(US)");
        assert_eq!(info.device_id, "DEV111");
        assert_eq!(info.alias, "Lamp");
        assert_eq!(info.relay_state, 0); // default
        assert!(info.children.is_empty());
    }

    // 21. SysInfo default fallback values
    #[test]
    fn test_deserialize_sysinfo_defaults() {
        let json_str = "{}";
        let info: GetSysInfoResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(info.mac, "");
        assert_eq!(info.model, "");
        assert_eq!(info.alias, "");
        assert_eq!(info.relay_state, 0);
        assert_eq!(info.on_time, 0);
        assert_eq!(info.rssi, 0);
        assert_eq!(info.device_id, "");
    }

    // 22. Normalize with zero values
    #[test]
    fn test_normalize_all_zeroes() {
        let mut resp = GetRealtimeResponse {
            current: -1.0,
            voltage: -1.0,
            power: -1.0,
            total: -1.0,
            current_ma: 0.0,
            voltage_mv: 0.0,
            power_mw: 0.0,
            total_wh: 0.0,
        };
        resp.normalize();
        assert_eq!(resp.current, 0.0);
        assert_eq!(resp.voltage, 0.0);
        assert_eq!(resp.power, 0.0);
        assert_eq!(resp.total, 0.0);
    }

    // 14. Encrypt/decrypt empty payload
    #[test]
    fn test_encrypt_decrypt_empty() {
        let empty: &[u8] = b"";
        let encrypted = encrypt(empty);
        assert!(encrypted.is_empty());
        let decrypted = decrypt(&encrypted);
        assert!(decrypted.is_empty());
    }

    // 15. Encrypt/decrypt large payload
    #[test]
    fn test_encrypt_decrypt_large_payload() {
        let large = vec![0x42u8; 16384];
        let encrypted = encrypt(&large);
        assert_eq!(encrypted.len(), large.len());
        let decrypted = decrypt(&encrypted);
        assert_eq!(decrypted, large);
    }

    // 16. KasaClient::new host format handling
    #[test]
    fn test_client_new_host_variations() {
        let c1 = KasaClient::new(&KasaClientConfig {
            host: "192.168.1.50".to_string(),
        });
        assert_eq!(c1.addr, "192.168.1.50:9999");

        let c2 = KasaClient::new(&KasaClientConfig {
            host: "192.168.1.50:12345".to_string(),
        });
        assert_eq!(c2.addr, "192.168.1.50:12345");
    }

    // 17. Server sends short header then EOF
    #[tokio::test]
    async fn test_mock_tcp_server_short_header_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 100];
            let _ = socket.read(&mut buf).await;
            // Write only 2 bytes instead of 4, then close
            let _ = socket.write_all(&[0, 1]).await;
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };
        let res = client.request(&serde_json::json!({})).await;
        assert!(res.is_err(), "Must error on premature EOF during header read");
    }

    // 18. Server sends length header then premature EOF
    #[tokio::test]
    async fn test_mock_tcp_server_short_body_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 100];
            let _ = socket.read(&mut buf).await;
            // Send length prefix saying 50 bytes, but send only 5 bytes
            let _ = socket.write_all(&pack_int(50)).await;
            let _ = socket.write_all(&[1, 2, 3, 4, 5]).await;
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };
        let res = client.request(&serde_json::json!({})).await;
        assert!(res.is_err(), "Must error on premature EOF during body read");
    }

    // 19. Server sends response missing requested service or command
    #[tokio::test]
    async fn test_mock_tcp_server_missing_service_in_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            let _ = socket.read_exact(&mut len_buf).await;
            let msg_len = unpack_int(&len_buf) as usize;
            let mut req_buf = vec![0u8; msg_len];
            let _ = socket.read_exact(&mut req_buf).await;

            // Send unexpected JSON structure
            let resp = serde_json::json!({ "other_service": {} });
            let enc = encrypt(&serde_json::to_vec(&resp).unwrap());
            let _ = socket.write_all(&pack_int(enc.len() as u32)).await;
            let _ = socket.write_all(&enc).await;
        });

        let client = KasaClient {
            addr: format!("127.0.0.1:{port}"),
        };
        let res = client.rpc("system", "get_sysinfo", None, serde_json::json!({})).await;
        match res {
            Err(KasaError::UnexpectedResponse) => {}
            other => panic!("Expected UnexpectedResponse error, got {:?}", other),
        }
    }

    // 20. SysInfo deserialization with extra unknown fields
    #[test]
    fn test_deserialize_sysinfo_with_extra_fields() {
        let json_str = r#"{
            "sw_ver": "2.0.0",
            "hw_ver": "1.0",
            "model": "HS100(US)",
            "deviceId": "DEV111",
            "alias": "Lamp",
            "extra_future_field": 12345,
            "another_field": "hello"
        }"#;

        let info: GetSysInfoResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(info.model, "HS100(US)");
        assert_eq!(info.device_id, "DEV111");
        assert_eq!(info.alias, "Lamp");
        assert_eq!(info.relay_state, 0); // default
        assert!(info.children.is_empty());
    }

    // 21. SysInfo default fallback values
    #[test]
    fn test_deserialize_sysinfo_defaults() {
        let json_str = "{}";
        let info: GetSysInfoResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(info.mac, "");
        assert_eq!(info.model, "");
        assert_eq!(info.alias, "");
        assert_eq!(info.relay_state, 0);
        assert_eq!(info.on_time, 0);
        assert_eq!(info.rssi, 0);
        assert_eq!(info.device_id, "");
    }

    // 22. Normalize with zero values
    #[test]
    fn test_normalize_all_zeroes() {
        let mut resp = GetRealtimeResponse {
            current: -1.0,
            voltage: -1.0,
            power: -1.0,
            total: -1.0,
            current_ma: 0.0,
            voltage_mv: 0.0,
            power_mw: 0.0,
            total_wh: 0.0,
        };
        resp.normalize();
        assert_eq!(resp.current, 0.0);
        assert_eq!(resp.voltage, 0.0);
        assert_eq!(resp.power, 0.0);
        assert_eq!(resp.total, 0.0);
    }
}

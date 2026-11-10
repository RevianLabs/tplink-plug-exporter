mod exporter;
mod kasa;
mod server;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "tplink-plug-exporter")]
#[command(about = "Export TP-Link Smart Plug metrics to Prometheus")]
pub struct Cli {
    /// Listen address for the exporter (e.g. :9233 or 0.0.0.0:9233)
    #[arg(
        long = "metrics.listen-addr",
        visible_alias = "listen-addr",
        default_value = ":9233"
    )]
    pub listen_addr: String,
}

pub fn normalize_addr(addr: &str) -> String {
    if addr.starts_with(':') {
        format!("0.0.0.0{}", addr)
    } else {
        addr.to_string()
    }
}

pub fn parse_cli_args<I, T>(args: I) -> Cli
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    // Convert any single-dash Go-style flag like `-metrics.listen-addr` to `--metrics.listen-addr`
    let transformed: Vec<std::ffi::OsString> = args
        .into_iter()
        .map(|item| {
            let os_str: std::ffi::OsString = item.into();
            if let Some(s) = os_str.to_str() {
                if s.starts_with("-metrics.") {
                    return std::ffi::OsString::from(format!("-{}", s));
                }
            }
            os_str
        })
        .collect();

    Cli::parse_from(transformed)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = parse_cli_args(std::env::args_os());
    let addr = normalize_addr(&cli.listen_addr);
    let app = server::create_router();

    tracing::info!("Accepting Prometheus Requests on {}", addr);
    println!("Accepting Prometheus Requests on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_addr() {
        assert_eq!(normalize_addr(":9233"), "0.0.0.0:9233");
        assert_eq!(normalize_addr(":8080"), "0.0.0.0:8080");
        assert_eq!(normalize_addr("127.0.0.1:9233"), "127.0.0.1:9233");
        assert_eq!(normalize_addr("0.0.0.0:9233"), "0.0.0.0:9233");
        assert_eq!(normalize_addr("localhost:9233"), "localhost:9233");
        assert_eq!(normalize_addr("[::1]:9233"), "[::1]:9233");
    }

    #[test]
    fn test_cli_parsing_default() {
        let cli = parse_cli_args(["tplink-plug-exporter"]);
        assert_eq!(cli.listen_addr, ":9233");
    }

    #[test]
    fn test_cli_parsing_double_dash() {
        let cli = parse_cli_args([
            "tplink-plug-exporter",
            "--metrics.listen-addr",
            "127.0.0.1:8080",
        ]);
        assert_eq!(cli.listen_addr, "127.0.0.1:8080");
    }

    #[test]
    fn test_cli_parsing_single_dash_go_style() {
        let cli = parse_cli_args(["tplink-plug-exporter", "-metrics.listen-addr", ":9999"]);
        assert_eq!(cli.listen_addr, ":9999");
    }

    #[test]
    fn test_cli_parsing_alias() {
        let cli = parse_cli_args(["tplink-plug-exporter", "--listen-addr", "0.0.0.0:9100"]);
        assert_eq!(cli.listen_addr, "0.0.0.0:9100");
    }
}

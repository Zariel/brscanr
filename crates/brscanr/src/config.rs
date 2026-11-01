use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use brother_scan::{Limits, Source};

pub struct Config {
    pub printer: Ipv4Addr,
    pub callback: SocketAddrV4,
    pub destination: String,
    pub insecure_tls: bool,
    pub listen: SocketAddr,
    pub spool: PathBuf,
    pub paperless_url: url::Url,
    pub paperless_token: String,
    pub button_source: Source,
    pub min_free_bytes: u64,
    pub max_spool_bytes: u64,
    pub limits: Limits,
    pub retention_days: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let required = |name| std::env::var(name).with_context(|| format!("set {name}"));
        let parse = |name, fallback: &str| std::env::var(name).unwrap_or_else(|_| fallback.into());
        let paperless_url = url::Url::parse(&required("BRSCANR_PAPERLESS_URL")?)?;
        if !["http", "https"].contains(&paperless_url.scheme())
            || paperless_url.cannot_be_a_base()
            || !paperless_url.username().is_empty()
            || paperless_url.password().is_some()
            || paperless_url.query().is_some()
            || paperless_url.fragment().is_some()
            || !paperless_url.path().ends_with('/')
        {
            bail!(
                "BRSCANR_PAPERLESS_URL must be an HTTP(S) base URL ending in '/' without credentials, query or fragment"
            );
        }
        let paperless_token = if let Ok(path) = std::env::var("BRSCANR_PAPERLESS_TOKEN_FILE") {
            std::fs::read_to_string(path)?.trim().to_owned()
        } else {
            required("BRSCANR_PAPERLESS_TOKEN")?
        };
        if paperless_token.is_empty() || paperless_token.contains(['\r', '\n']) {
            bail!("invalid Paperless token");
        }
        let button_source = match parse("BRSCANR_BUTTON_SOURCE", "feeder").as_str() {
            "feeder" => Source::Feeder,
            "platen" => Source::Platen,
            _ => bail!("BRSCANR_BUTTON_SOURCE must be feeder or platen"),
        };
        let limits = Limits {
            max_pages: parse("BRSCANR_MAX_PAGES", "100").parse()?,
            max_bytes: parse("BRSCANR_MAX_SCAN_BYTES", "268435456").parse()?,
            ..Limits::default()
        };
        if limits.max_pages == 0 || limits.max_pages > 1000 || limits.max_bytes == 0 {
            bail!("invalid acquisition limits");
        }
        Ok(Self {
            printer: required("BRSCANR_PRINTER_IP")?.parse()?,
            callback: required("BRSCANR_CALLBACK")?.parse()?,
            destination: parse("BRSCANR_DESTINATION", "Paperless"),
            insecure_tls: parse("BRSCANR_INSECURE_PRINTER_TLS", "false").parse()?,
            listen: parse("BRSCANR_LISTEN", "0.0.0.0:8080").parse()?,
            spool: parse("BRSCANR_SPOOL", "/var/lib/brscanr").into(),
            paperless_url,
            paperless_token,
            button_source,
            limits,
            min_free_bytes: parse("BRSCANR_MIN_FREE_BYTES", "1073741824").parse()?,
            max_spool_bytes: parse("BRSCANR_MAX_SPOOL_BYTES", "8589934592").parse()?,
            retention_days: parse("BRSCANR_RETENTION_DAYS", "7").parse()?,
        })
    }
}

use anyhow::Result;
use prometheus::{
    Encoder, IntCounter, IntGauge, Registry, TextEncoder, process_collector::ProcessCollector,
};

pub struct Metrics {
    pub registry: Registry,
    pub captured: IntCounter,
    pub failed: IntCounter,
    pub renew_failures: IntCounter,
    pub lease_until: IntGauge,
    pub last_callback: IntGauge,
    pub queue: IntGauge,
    pub oldest: IntGauge,
    pub spool_bytes: IntGauge,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();
        registry.register(Box::new(ProcessCollector::for_self()))?;
        let counter = |name, help| -> Result<IntCounter> {
            let c = IntCounter::new(name, help)?;
            registry.register(Box::new(c.clone()))?;
            Ok(c)
        };
        let gauge = |name, help| -> Result<IntGauge> {
            let g = IntGauge::new(name, help)?;
            registry.register(Box::new(g.clone()))?;
            Ok(g)
        };
        Ok(Self {
            captured: counter("brscanr_captured_total", "Complete acquisitions")?,
            failed: counter("brscanr_scan_failures_total", "Incomplete acquisitions")?,
            renew_failures: counter(
                "brscanr_registration_failures_total",
                "Failed Phoenix renewals",
            )?,
            lease_until: gauge(
                "brscanr_registration_expires_seconds",
                "Estimated registration expiry, Unix seconds",
            )?,
            last_callback: gauge(
                "brscanr_last_callback_seconds",
                "Last validated callback, Unix seconds; zero means unobserved",
            )?,
            queue: gauge(
                "brscanr_undelivered_jobs",
                "Retained jobs awaiting import or reconciliation",
            )?,
            oldest: gauge(
                "brscanr_oldest_undelivered_seconds",
                "Age of oldest undelivered job",
            )?,
            spool_bytes: gauge("brscanr_spool_bytes", "Observed spool file bytes")?,
            registry,
        })
    }
    pub fn render(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        TextEncoder::new().encode(&self.registry.gather(), &mut bytes)?;
        Ok(bytes)
    }
}

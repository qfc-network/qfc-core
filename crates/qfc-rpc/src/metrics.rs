//! RPC observability metrics (T2.2).
//!
//! Lock-light Prometheus-style metrics for the JSON-RPC server:
//! - per-method latency histograms (buckets chosen for p50/p99/p999 queries),
//! - an in-flight request gauge,
//! - error counters labelled by method and JSON-RPC error code.
//!
//! The metrics are recorded by [`MetricsMiddleware`], a jsonrpsee RPC
//! middleware, and rendered into the node's Prometheus exporter via
//! [`RpcMetrics::render_prometheus`]. No `prometheus` crate dependency — the
//! node exporter (qfc-node/src/metrics.rs) hand-renders the text format, and
//! this module follows the same convention.

use futures::future::BoxFuture;
use jsonrpsee::core::server::MethodResponse;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::types::Request;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Histogram bucket upper bounds in seconds.
///
/// Spans 500µs (cache-hit reads) to 10s (worst-case range scans) with enough
/// resolution around 1–500ms for meaningful p50/p99/p999 estimates via
/// `histogram_quantile()`.
pub const LATENCY_BUCKETS_SECONDS: [f64; 14] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Per-method counters and latency histogram.
#[derive(Default)]
struct MethodStats {
    /// Total completed requests.
    count: AtomicU64,
    /// Sum of observed durations in nanoseconds (rendered as seconds in `_sum`).
    sum_nanos: AtomicU64,
    /// Cumulative-style bucket counts are computed at render time; these are
    /// per-bucket (non-cumulative) increments to keep recording a single
    /// fetch_add.
    buckets: [AtomicU64; LATENCY_BUCKETS_SECONDS.len()],
    /// Observations larger than the last bucket bound (counted in `+Inf` only).
    overflow: AtomicU64,
    /// Error counts by JSON-RPC error code.
    errors_by_code: RwLock<HashMap<i32, u64>>,
}

impl MethodStats {
    fn record(&self, elapsed: Duration, error_code: Option<i32>) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);

        let secs = elapsed.as_secs_f64();
        match LATENCY_BUCKETS_SECONDS.iter().position(|&le| secs <= le) {
            Some(idx) => {
                self.buckets[idx].fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.overflow.fetch_add(1, Ordering::Relaxed);
            }
        }

        if let Some(code) = error_code {
            *self.errors_by_code.write().entry(code).or_insert(0) += 1;
        }
    }
}

/// Shared RPC metrics registry.
///
/// One instance per node, shared between the RPC middleware (writer) and the
/// Prometheus exporter (reader).
#[derive(Default)]
pub struct RpcMetrics {
    /// Requests currently being processed.
    in_flight: AtomicU64,
    /// Per-method stats, keyed by JSON-RPC method name.
    methods: RwLock<HashMap<String, Arc<MethodStats>>>,
}

impl RpcMetrics {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn method_stats(&self, method: &str) -> Arc<MethodStats> {
        if let Some(stats) = self.methods.read().get(method) {
            return stats.clone();
        }
        self.methods
            .write()
            .entry(method.to_owned())
            .or_default()
            .clone()
    }

    /// Increment the in-flight gauge (request started).
    pub fn request_started(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the in-flight gauge (request finished) and record the
    /// observation.
    pub fn request_finished(&self, method: &str, elapsed: Duration, error_code: Option<i32>) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.method_stats(method).record(elapsed, error_code);
    }

    /// Current number of in-flight requests.
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Append all RPC metrics to `out` in Prometheus text exposition format.
    pub fn render_prometheus(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP qfc_rpc_requests_in_flight JSON-RPC requests currently being processed."
        );
        let _ = writeln!(out, "# TYPE qfc_rpc_requests_in_flight gauge");
        let _ = writeln!(out, "qfc_rpc_requests_in_flight {}", self.in_flight());

        // Snapshot method map (Arc clones; cheap) so we don't hold the lock
        // while formatting.
        let methods: Vec<(String, Arc<MethodStats>)> = {
            let guard = self.methods.read();
            let mut v: Vec<_> = guard.iter().map(|(k, s)| (k.clone(), s.clone())).collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };

        let _ = writeln!(
            out,
            "# HELP qfc_rpc_requests_total Total JSON-RPC requests completed, by method."
        );
        let _ = writeln!(out, "# TYPE qfc_rpc_requests_total counter");
        for (method, stats) in &methods {
            let _ = writeln!(
                out,
                "qfc_rpc_requests_total{{method=\"{method}\"}} {}",
                stats.count.load(Ordering::Relaxed)
            );
        }

        let _ = writeln!(
            out,
            "# HELP qfc_rpc_request_duration_seconds JSON-RPC request latency, by method."
        );
        let _ = writeln!(out, "# TYPE qfc_rpc_request_duration_seconds histogram");
        for (method, stats) in &methods {
            let mut cumulative = 0u64;
            for (idx, le) in LATENCY_BUCKETS_SECONDS.iter().enumerate() {
                cumulative += stats.buckets[idx].load(Ordering::Relaxed);
                let _ = writeln!(
                    out,
                    "qfc_rpc_request_duration_seconds_bucket{{method=\"{method}\",le=\"{le}\"}} {cumulative}"
                );
            }
            let total = cumulative + stats.overflow.load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "qfc_rpc_request_duration_seconds_bucket{{method=\"{method}\",le=\"+Inf\"}} {total}"
            );
            let sum_secs = stats.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9;
            let _ = writeln!(
                out,
                "qfc_rpc_request_duration_seconds_sum{{method=\"{method}\"}} {sum_secs:.9}"
            );
            let _ = writeln!(
                out,
                "qfc_rpc_request_duration_seconds_count{{method=\"{method}\"}} {}",
                stats.count.load(Ordering::Relaxed)
            );
        }

        let _ = writeln!(
            out,
            "# HELP qfc_rpc_errors_total JSON-RPC error responses, by method and error code."
        );
        let _ = writeln!(out, "# TYPE qfc_rpc_errors_total counter");
        for (method, stats) in &methods {
            let errors = stats.errors_by_code.read();
            let mut codes: Vec<_> = errors.iter().collect();
            codes.sort_by_key(|(code, _)| **code);
            for (code, count) in codes {
                let _ = writeln!(
                    out,
                    "qfc_rpc_errors_total{{method=\"{method}\",code=\"{code}\"}} {count}"
                );
            }
        }
    }
}

/// RPC service wrapper that times each call and classifies the response.
///
/// Install via `RpcServiceBuilder::new().layer_fn(move |s| MetricsMiddleware::new(s, metrics.clone()))`
/// and `ServerBuilder::set_rpc_middleware`.
#[derive(Clone)]
pub struct MetricsMiddleware<S> {
    inner: S,
    metrics: Arc<RpcMetrics>,
}

impl<S> MetricsMiddleware<S> {
    /// Wrap an RPC service, recording into the given registry.
    pub fn new(inner: S, metrics: Arc<RpcMetrics>) -> Self {
        Self { inner, metrics }
    }
}

impl<'a, S> RpcServiceT<'a> for MetricsMiddleware<S>
where
    S: RpcServiceT<'a> + Send + Sync + 'a,
{
    type Future = BoxFuture<'a, MethodResponse>;

    fn call(&self, request: Request<'a>) -> Self::Future {
        let metrics = self.metrics.clone();
        let method = request.method_name().to_owned();
        metrics.request_started();
        let fut = self.inner.call(request);

        Box::pin(async move {
            let start = Instant::now();
            let response = fut.await;
            metrics.request_finished(&method, start.elapsed(), response.as_error_code());
            response
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_renders_basic_metrics() {
        let metrics = RpcMetrics::new();

        metrics.request_started();
        metrics.request_finished("eth_blockNumber", Duration::from_micros(800), None);
        metrics.request_started();
        metrics.request_finished("eth_blockNumber", Duration::from_millis(30), None);
        metrics.request_started();
        metrics.request_finished("eth_call", Duration::from_millis(120), Some(-32000));

        let mut out = String::new();
        metrics.render_prometheus(&mut out);

        // Metric families present
        assert!(out.contains("qfc_rpc_requests_in_flight 0"));
        assert!(out.contains("qfc_rpc_requests_total{method=\"eth_blockNumber\"} 2"));
        assert!(out.contains("qfc_rpc_requests_total{method=\"eth_call\"} 1"));
        assert!(out.contains("qfc_rpc_errors_total{method=\"eth_call\",code=\"-32000\"} 1"));
        assert!(
            out.contains("qfc_rpc_request_duration_seconds_count{method=\"eth_blockNumber\"} 2")
        );
        assert!(out.contains("qfc_rpc_request_duration_seconds_sum{method=\"eth_blockNumber\"}"));
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let metrics = RpcMetrics::new();
        metrics.request_started();
        metrics.request_finished("eth_call", Duration::from_micros(400), None); // <= 0.0005
        metrics.request_started();
        metrics.request_finished("eth_call", Duration::from_millis(2), None); // <= 0.0025
        metrics.request_started();
        metrics.request_finished("eth_call", Duration::from_secs(60), None); // > 10s -> +Inf only

        let mut out = String::new();
        metrics.render_prometheus(&mut out);

        assert!(out.contains(
            "qfc_rpc_request_duration_seconds_bucket{method=\"eth_call\",le=\"0.0005\"} 1"
        ));
        assert!(out.contains(
            "qfc_rpc_request_duration_seconds_bucket{method=\"eth_call\",le=\"0.0025\"} 2"
        ));
        // Largest finite bucket sees both fast observations, not the 60s one
        assert!(out
            .contains("qfc_rpc_request_duration_seconds_bucket{method=\"eth_call\",le=\"10\"} 2"));
        assert!(out.contains(
            "qfc_rpc_request_duration_seconds_bucket{method=\"eth_call\",le=\"+Inf\"} 3"
        ));
        assert!(out.contains("qfc_rpc_request_duration_seconds_count{method=\"eth_call\"} 3"));
    }

    #[test]
    fn in_flight_gauge_tracks_outstanding_requests() {
        let metrics = RpcMetrics::new();
        metrics.request_started();
        metrics.request_started();
        assert_eq!(metrics.in_flight(), 2);
        metrics.request_finished("eth_call", Duration::from_millis(1), None);
        assert_eq!(metrics.in_flight(), 1);
    }
}

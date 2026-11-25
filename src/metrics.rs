//! Prometheus metrics (`GET /metrics`, text exposition format 0.0.4).
//!
//! Counters live in one process-wide registry; gauges that describe shared state (leadership, conflicts,
//! routed backends) are computed when the endpoint is scraped. The public nginx listener does not expose
//! `/metrics`; scrape the service port from the internal network.

use std::{
    collections::BTreeMap,
    fmt::Write,
    sync::{Mutex, OnceLock},
};

/// Counter values keyed by `name{labels}`.
fn registry() -> &'static Mutex<BTreeMap<(&'static str, String), u64>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<(&'static str, String), u64>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Help text of every counter, in exposition order.
const COUNTERS: &[(&str, &str)] = &[
    ("router_routing_requests_total", "Routing API requests by entity and outcome."),
    ("router_sync_pages_applied_total", "Change pages applied from the backends' streams."),
    ("router_sync_resyncs_total", "Full resyncs (snapshot + replay) of a stream."),
    ("router_sync_errors_total", "Failed sync requests or applications, by entity."),
    ("router_jwks_refresh_failures_total", "Failed downloads of the Microsoft signing keys."),
];

fn label_text(labels: &[(&str, &str)]) -> String {
    labels.iter().map(|(k, v)| format!("{k}=\"{}\"", escape(v))).collect::<Vec<_>>().join(",")
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Adds one to counter `name` with `labels`. `name` must be listed in [`COUNTERS`].
pub fn inc(name: &'static str, labels: &[(&str, &str)]) {
    debug_assert!(COUNTERS.iter().any(|(n, _)| *n == name), "unknown counter {name}");
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    *reg.entry((name, label_text(labels))).or_insert(0) += 1;
}

/// A gauge sample computed at scrape time.
pub struct Gauge {
    pub name: &'static str,
    pub help: &'static str,
    pub samples: Vec<(String, f64)>,
}

impl Gauge {
    pub fn single(name: &'static str, help: &'static str, value: f64) -> Gauge {
        Gauge { name, help, samples: vec![(String::new(), value)] }
    }
}

/// Exposition text of every counter plus `gauges`.
pub fn render(gauges: &[Gauge]) -> String {
    let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    let mut out = String::new();
    for (name, help) in COUNTERS {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
        for ((n, labels), value) in reg.iter() {
            if n == name {
                let _ = writeln!(out, "{}{} {value}", name, braces(labels));
            }
        }
    }
    for g in gauges {
        let _ = writeln!(out, "# HELP {} {}\n# TYPE {} gauge", g.name, g.help, g.name);
        for (labels, value) in &g.samples {
            let _ = writeln!(out, "{}{} {value}", g.name, braces(labels));
        }
    }
    out
}

fn braces(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{labels}}}")
    }
}

/// `entity="users"` style label text for a gauge sample.
pub fn labels(pairs: &[(&str, &str)]) -> String {
    label_text(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition() {
        inc("router_routing_requests_total", &[("entity", "users"), ("outcome", "ok")]);
        inc("router_routing_requests_total", &[("entity", "users"), ("outcome", "ok")]);
        let text = render(&[Gauge::single("router_sync_leader", "Leader.", 1.0)]);
        assert!(text.contains("# TYPE router_routing_requests_total counter"));
        assert!(text.contains("router_routing_requests_total{entity=\"users\",outcome=\"ok\"} "));
        assert!(text.contains("router_sync_leader 1\n"));
        assert_eq!(labels(&[("k", "a\"b")]), "k=\"a\\\"b\"");
    }
}

//! Minimal Prometheus text-format parsing — just enough to read the
//! qfc-node exporter output. Deliberately not a metrics library: the
//! watchdog needs exact-name lookups and per-label sums, nothing more.

/// A parsed metrics exposition: `(metric name, value)` pairs with labels
/// stripped. Multiple samples of the same name (one per label set) are all
/// kept, so per-CF gauges can be summed.
#[derive(Debug, Default)]
pub struct MetricsText {
    samples: Vec<(String, f64)>,
}

impl MetricsText {
    pub fn parse(text: &str) -> Self {
        let mut samples = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // `name value`, `name{labels} value`, optional trailing timestamp.
            let (name, rest) = match line.find('{') {
                Some(brace) => match line[brace..].find('}') {
                    Some(close) => (&line[..brace], &line[brace + close + 1..]),
                    None => continue,
                },
                None => match line.find(char::is_whitespace) {
                    Some(ws) => (&line[..ws], &line[ws..]),
                    None => continue,
                },
            };
            let Some(value) = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
            else {
                continue;
            };
            samples.push((name.to_string(), value));
        }
        Self { samples }
    }

    /// First sample with this exact name. Use for unlabeled gauges.
    pub fn value(&self, name: &str) -> Option<f64> {
        self.samples
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
    }

    /// Sum of all samples with this name (`None` when the series is absent).
    /// Use for labeled per-CF gauges like `qfc_rocksdb_pending_compaction_bytes`.
    pub fn sum(&self, name: &str) -> Option<f64> {
        let mut total = 0.0;
        let mut found = false;
        for (n, v) in &self.samples {
            if n == name {
                total += v;
                found = true;
            }
        }
        found.then_some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unlabeled_labeled_and_skips_comments() {
        let text = "\
# HELP qfc_block_height Current block height.
# TYPE qfc_block_height gauge
qfc_block_height 1234
qfc_time_since_last_block_seconds 4.250

qfc_rocksdb_pending_compaction_bytes{cf=\"state\"} 100
qfc_rocksdb_pending_compaction_bytes{cf=\"blocks\"} 250
qfc_node_info{version=\"2.2.3\"} 1
garbage line without value
qfc_bad_value not_a_number
";
        let m = MetricsText::parse(text);
        assert_eq!(m.value("qfc_block_height"), Some(1234.0));
        assert_eq!(m.value("qfc_time_since_last_block_seconds"), Some(4.25));
        // Labeled series: value() returns the first sample, sum() adds all.
        assert_eq!(m.value("qfc_rocksdb_pending_compaction_bytes"), Some(100.0));
        assert_eq!(m.sum("qfc_rocksdb_pending_compaction_bytes"), Some(350.0));
        assert_eq!(m.value("qfc_node_info"), Some(1.0));
        assert_eq!(m.value("qfc_missing"), None);
        assert_eq!(m.sum("qfc_missing"), None);
        assert_eq!(m.value("qfc_bad_value"), None);
    }

    #[test]
    fn handles_timestamps_and_whitespace() {
        let m = MetricsText::parse("qfc_x  7 1700000000000\n\tqfc_y{a=\"b\"}\t3.5\n");
        assert_eq!(m.value("qfc_x"), Some(7.0));
        assert_eq!(m.value("qfc_y"), Some(3.5));
    }
}

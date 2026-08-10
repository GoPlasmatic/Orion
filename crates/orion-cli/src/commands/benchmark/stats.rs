use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    pub scenario: String,
    pub total_requests: usize,
    pub concurrency: usize,
    pub successful: usize,
    pub failed: usize,
    pub success_rate_pct: f64,
    pub total_duration_ms: f64,
    pub throughput_rps: f64,
    pub latency: LatencyStats,
}

#[derive(Debug, Serialize)]
pub struct LatencyStats {
    pub min_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

pub struct RequestResult {
    pub duration: Duration,
    pub success: bool,
}

impl BenchmarkReport {
    pub fn from_results(
        results: &[RequestResult],
        total_elapsed: Duration,
        scenario: &str,
        concurrency: usize,
    ) -> Self {
        let total = results.len();
        let successful = results.iter().filter(|r| r.success).count();
        let failed = total - successful;
        let success_rate_pct = if total > 0 {
            (successful as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let total_duration_ms = total_elapsed.as_secs_f64() * 1000.0;
        let throughput_rps = if total_elapsed.as_secs_f64() > 0.0 {
            successful as f64 / total_elapsed.as_secs_f64()
        } else {
            0.0
        };

        let mut durations: Vec<Duration> = results
            .iter()
            .filter(|r| r.success)
            .map(|r| r.duration)
            .collect();
        durations.sort();

        let latency = if durations.is_empty() {
            LatencyStats {
                min_ms: 0.0,
                max_ms: 0.0,
                mean_ms: 0.0,
                p50_ms: 0.0,
                p90_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
            }
        } else {
            let sum: Duration = durations.iter().sum();
            let mean = sum.as_secs_f64() * 1000.0 / durations.len() as f64;

            LatencyStats {
                min_ms: durations.first().unwrap().as_secs_f64() * 1000.0,
                max_ms: durations.last().unwrap().as_secs_f64() * 1000.0,
                mean_ms: mean,
                p50_ms: percentile(&durations, 50.0),
                p90_ms: percentile(&durations, 90.0),
                p95_ms: percentile(&durations, 95.0),
                p99_ms: percentile(&durations, 99.0),
            }
        };

        Self {
            scenario: scenario.to_string(),
            total_requests: total,
            concurrency,
            successful,
            failed,
            success_rate_pct,
            total_duration_ms,
            throughput_rps,
            latency,
        }
    }
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)].as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percentile_single_element() {
        let durations = vec![Duration::from_millis(10)];
        assert!((percentile(&durations, 50.0) - 10.0).abs() < 0.1);
        assert!((percentile(&durations, 99.0) - 10.0).abs() < 0.1);
    }

    #[test]
    fn test_percentile_multiple_elements() {
        let durations: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        // p50 of 1..=100 → index 50 → 51ms
        assert!((percentile(&durations, 50.0) - 50.5).abs() < 2.0);
        assert!((percentile(&durations, 99.0) - 99.0).abs() < 2.0);
    }

    #[test]
    fn test_percentile_empty() {
        let durations: Vec<Duration> = vec![];
        assert_eq!(percentile(&durations, 50.0), 0.0);
    }

    #[test]
    fn test_report_all_failures() {
        let results = vec![
            RequestResult {
                duration: Duration::from_millis(10),
                success: false,
            },
            RequestResult {
                duration: Duration::from_millis(20),
                success: false,
            },
        ];
        let report = BenchmarkReport::from_results(&results, Duration::from_secs(1), "test", 1);
        assert_eq!(report.successful, 0);
        assert_eq!(report.failed, 2);
        assert_eq!(report.latency.min_ms, 0.0);
    }

    #[test]
    fn test_report_mixed_results() {
        let results = vec![
            RequestResult {
                duration: Duration::from_millis(5),
                success: true,
            },
            RequestResult {
                duration: Duration::from_millis(10),
                success: true,
            },
            RequestResult {
                duration: Duration::from_millis(100),
                success: false,
            },
        ];
        let report = BenchmarkReport::from_results(&results, Duration::from_secs(1), "test", 2);
        assert_eq!(report.successful, 2);
        assert_eq!(report.failed, 1);
        assert_eq!(report.concurrency, 2);
        assert!(report.latency.min_ms > 4.0 && report.latency.min_ms < 6.0);
        assert!(report.latency.max_ms > 9.0 && report.latency.max_ms < 11.0);
    }
}

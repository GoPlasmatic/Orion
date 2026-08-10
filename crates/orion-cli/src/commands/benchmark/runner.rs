use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::client::OrionClient;

use super::stats::RequestResult;

#[allow(clippy::too_many_arguments)]
pub async fn run_benchmark(
    client: &OrionClient,
    channel: &str,
    payload: &Value,
    num_requests: usize,
    concurrency: usize,
    timeout_secs: u64,
    cancel: &CancellationToken,
    quiet: bool,
) -> Result<(Vec<RequestResult>, Duration)> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut join_set = JoinSet::new();

    let body = serde_json::json!({ "data": payload });
    let url_path = format!("/api/v1/data/{channel}");
    let wall_start = Instant::now();

    for _ in 0..num_requests {
        if cancel.is_cancelled() {
            break;
        }

        let permit = semaphore.clone().acquire_owned().await?;
        let client = client.clone();
        let path = url_path.clone();
        let body = body.clone();
        let completed = completed.clone();
        let total = num_requests;
        let show_progress = !quiet;
        let req_timeout = Duration::from_secs(timeout_secs);

        join_set.spawn(async move {
            let start = Instant::now();
            let result =
                tokio::time::timeout(req_timeout, client.post::<Value>(&path, &body)).await;
            let duration = start.elapsed();
            drop(permit);

            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            if show_progress {
                eprint!("\r  Progress: {done}/{total} requests");
            }

            let success = matches!(result, Ok(Ok(_)));
            RequestResult { duration, success }
        });
    }

    let mut results = Vec::with_capacity(num_requests);
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(r) => results.push(r),
            Err(_) => results.push(RequestResult {
                duration: Duration::ZERO,
                success: false,
            }),
        }
    }

    let total_elapsed = wall_start.elapsed();

    if !quiet {
        eprintln!();
    }

    Ok((results, total_elapsed))
}

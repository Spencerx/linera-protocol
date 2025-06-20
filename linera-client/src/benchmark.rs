// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, iter, sync::Arc};

use linera_base::{
    data_types::{Amount, Epoch},
    identifiers::{AccountOwner, ApplicationId, ChainId},
    listen_for_shutdown_signals,
    time::Instant,
};
use linera_core::{client::ChainClient, Environment};
use linera_execution::{
    committee::Committee,
    system::{Recipient, SystemOperation},
    Operation,
};
use linera_sdk::abis::fungible;
use num_format::{Locale, ToFormattedString};
use prometheus_parse::{HistogramCount, Scrape, Value};
use tokio::{
    runtime::Handle,
    sync::{mpsc, Barrier},
    task, time,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn, Instrument as _};

const PROXY_LATENCY_P99_THRESHOLD: f64 = 400.0;
const LATENCY_METRIC_PREFIX: &str = "linera_proxy_request_latency";

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkError {
    #[error("Failed to send message: {0}")]
    CrossbeamSendError(#[from] crossbeam_channel::SendError<()>),
    #[error("Failed to join task: {0}")]
    JoinError(#[from] task::JoinError),
    #[error("Chain client error: {0}")]
    ChainClient(#[from] linera_core::client::ChainClientError),
    #[error("Current histogram count is less than previous histogram count")]
    HistogramCountMismatch,
    #[error("Expected histogram value, got {0:?}")]
    ExpectedHistogramValue(Value),
    #[error("Expected untyped value, got {0:?}")]
    ExpectedUntypedValue(Value),
    #[error("Incomplete histogram data")]
    IncompleteHistogramData,
    #[error("Could not compute quantile")]
    CouldNotComputeQuantile,
    #[error("Bucket boundaries do not match: {0} vs {1}")]
    BucketBoundariesDoNotMatch(f64, f64),
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Io error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Previous histogram snapshot does not exist: {0}")]
    PreviousHistogramSnapshotDoesNotExist(String),
    #[error("No data available yet to calculate p99")]
    NoDataYetForP99Calculation,
    #[error("Unexpected empty bucket")]
    UnexpectedEmptyBucket,
    #[error("Failed to send message: {0}")]
    TokioSendError(#[from] mpsc::error::SendError<()>),
}

#[derive(Debug)]
struct HistogramSnapshot {
    buckets: Vec<HistogramCount>,
    count: f64,
    sum: f64,
}

pub struct Benchmark<Env: Environment> {
    _phantom: std::marker::PhantomData<Env>,
}

impl<Env: Environment> Benchmark<Env> {
    #[expect(clippy::too_many_arguments)]
    pub async fn run_benchmark(
        num_chains: usize,
        transactions_per_block: usize,
        bps: Option<usize>,
        chain_clients: HashMap<ChainId, ChainClient<Env>>,
        epoch: Epoch,
        blocks_infos: Vec<(ChainId, Vec<Operation>, AccountOwner)>,
        committee: Committee,
        health_check_endpoints: Option<String>,
    ) -> Result<(), BenchmarkError> {
        let shutdown_notifier = CancellationToken::new();
        tokio::spawn(listen_for_shutdown_signals(shutdown_notifier.clone()));

        let handle = Handle::current();
        // The bps control task will control the BPS from the threads. `crossbeam_channel` is used
        // for two reasons:
        // 1. it allows bounded channels with zero sized buffers.
        // 2. it blocks the current thread if the message can't be sent, which is exactly
        //    what we want to happen here. `tokio::sync::mpsc` doesn't do that. `std::sync::mpsc`
        //    does, but it is slower than `crossbeam_channel`.
        // Number 1 is the main reason. `tokio::sync::mpsc` doesn't allow 0 sized buffers.
        // With a channel with a buffer of size 1 or larger, even if we have already reached
        // the desired BPS, the tasks would continue sending block proposals until the channel's
        // buffer is filled, which would cause us to not properly control the BPS rate.
        let (sender, receiver) = crossbeam_channel::bounded(0);
        let bps_control_task = task::spawn_blocking(move || {
            handle.block_on(async move {
                let mut recv_count = 0;
                let mut start = time::Instant::now();
                while let Ok(()) = receiver.recv() {
                    recv_count += 1;
                    if recv_count == num_chains {
                        let elapsed = start.elapsed();
                        if let Some(bps) = bps {
                            let tps =
                                (bps * transactions_per_block).to_formatted_string(&Locale::en);
                            let bps = bps.to_formatted_string(&Locale::en);
                            if elapsed > time::Duration::from_secs(1) {
                                warn!(
                                    "Failed to achieve {} BPS/{} TPS in {} ms",
                                    bps,
                                    tps,
                                    elapsed.as_millis(),
                                );
                            } else {
                                time::sleep(time::Duration::from_secs(1) - elapsed).await;
                                info!(
                                    "Achieved {} BPS/{} TPS in {} ms",
                                    bps,
                                    tps,
                                    elapsed.as_millis(),
                                );
                            }
                        } else {
                            let achieved_bps = num_chains as f64 / elapsed.as_secs_f64();
                            info!(
                                "Achieved {} BPS/{} TPS in {} ms",
                                achieved_bps,
                                achieved_bps * transactions_per_block as f64,
                                elapsed.as_millis(),
                            );
                        }

                        recv_count = 0;
                        start = time::Instant::now();
                    }
                }

                info!("Exiting logging task...");
            })
        });

        let (bps_tasks_logger_sender, mut bps_tasks_logger_receiver) = mpsc::channel(num_chains);
        let bps_tasks_logger_task = task::spawn(async move {
            let mut tasks_running = 0;
            while let Some(()) = bps_tasks_logger_receiver.recv().await {
                tasks_running += 1;
                info!("{}/{} tasks ready to start", tasks_running, num_chains);
                if tasks_running == num_chains {
                    info!("All tasks are ready to start");
                    break;
                }
            }
        });

        let mut bps_remainder = bps.unwrap_or_default() % num_chains;
        let bps_share = bps.map(|bps| bps / num_chains);

        let barrier = Arc::new(Barrier::new(num_chains));
        let mut join_set = task::JoinSet::<Result<(), BenchmarkError>>::new();
        for (chain_id, operations, chain_owner) in blocks_infos {
            let bps_share = if bps_remainder > 0 {
                bps_remainder -= 1;
                bps_share.map(|share| share + 1)
            } else {
                bps_share
            };

            let shutdown_notifier = shutdown_notifier.clone();
            let sender = sender.clone();
            let handle = Handle::current();
            let committee = committee.clone();
            let chain_client = chain_clients[&chain_id].clone();
            let bps_tasks_logger_sender = bps_tasks_logger_sender.clone();
            let inner_barrier = barrier.clone();
            chain_client.process_inbox().await?;
            join_set.spawn_blocking(move || {
                handle.block_on(
                    async move {
                        Box::pin(Self::run_benchmark_internal(
                            chain_owner,
                            bps_share,
                            operations,
                            epoch,
                            chain_client,
                            shutdown_notifier,
                            sender,
                            committee,
                            bps_tasks_logger_sender,
                            inner_barrier,
                        ))
                        .await?;

                        Ok(())
                    }
                    .instrument(tracing::info_span!(
                        "benchmark_chain_id",
                        chain_id = format!("{:?}", chain_id)
                    )),
                )
            });
        }

        let metrics_watcher =
            Self::create_metrics_watcher(health_check_endpoints, shutdown_notifier.clone()).await?;
        join_set
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        drop(sender);
        info!("All benchmark tasks completed");
        bps_control_task.await?;
        if let Some(metrics_watcher) = metrics_watcher {
            metrics_watcher.await??;
        }
        bps_tasks_logger_task.await?;

        Ok(())
    }

    async fn create_metrics_watcher(
        health_check_endpoints: Option<String>,
        shutdown_notifier: CancellationToken,
    ) -> Result<Option<task::JoinHandle<Result<(), BenchmarkError>>>, BenchmarkError> {
        if let Some(health_check_endpoints) = health_check_endpoints {
            let metrics_addresses = health_check_endpoints
                .split(',')
                .map(|address| format!("http://{}/metrics", address.trim()))
                .collect::<Vec<_>>();

            let mut previous_histogram_snapshots: HashMap<String, HistogramSnapshot> =
                HashMap::new();
            let scrapes = Self::get_scrapes(&metrics_addresses).await?;
            for (metrics_address, scrape) in scrapes {
                previous_histogram_snapshots.insert(
                    metrics_address,
                    Self::parse_histogram(&scrape, LATENCY_METRIC_PREFIX)?,
                );
            }

            let metrics_watcher: task::JoinHandle<Result<(), BenchmarkError>> = tokio::spawn(
                async move {
                    let mut health_interval = time::interval(time::Duration::from_secs(5));
                    let mut shutdown_interval = time::interval(time::Duration::from_secs(1));
                    loop {
                        tokio::select! {
                            biased;
                            _ = health_interval.tick() => {
                                let result = Self::validators_healthy(&metrics_addresses, &mut previous_histogram_snapshots).await;
                                if let Err(ref err) = result {
                                    info!("Shutting down benchmark due to error: {}", err);
                                    shutdown_notifier.cancel();
                                    break;
                                } else if !result? {
                                    info!("Shutting down benchmark due to unhealthy validators");
                                    shutdown_notifier.cancel();
                                    break;
                                }
                            }
                            _ = shutdown_interval.tick() => {
                                if shutdown_notifier.is_cancelled() {
                                    info!("Shutdown signal received, stopping metrics watcher");
                                    break;
                                }
                            }
                        }
                    }

                    Ok(())
                },
            );

            Ok(Some(metrics_watcher))
        } else {
            Ok(None)
        }
    }

    async fn validators_healthy(
        metrics_addresses: &[String],
        previous_histogram_snapshots: &mut HashMap<String, HistogramSnapshot>,
    ) -> Result<bool, BenchmarkError> {
        let scrapes = Self::get_scrapes(metrics_addresses).await?;
        for (metrics_address, scrape) in scrapes {
            let histogram = Self::parse_histogram(&scrape, LATENCY_METRIC_PREFIX)?;
            let diff = Self::diff_histograms(
                previous_histogram_snapshots.get(&metrics_address).ok_or(
                    BenchmarkError::PreviousHistogramSnapshotDoesNotExist(metrics_address.clone()),
                )?,
                &histogram,
            )?;
            let p99 = match Self::compute_quantile(&diff.buckets, diff.count, 0.99) {
                Ok(p99) => p99,
                Err(BenchmarkError::NoDataYetForP99Calculation) => {
                    info!(
                        "No data available yet to calculate p99 for {}",
                        metrics_address
                    );
                    continue;
                }
                Err(e) => {
                    error!("Error computing p99 for {}: {}", metrics_address, e);
                    return Err(e);
                }
            };

            let last_bucket_boundary = diff.buckets[diff.buckets.len() - 2].less_than;
            if p99 == f64::INFINITY {
                info!(
                    "{} -> Estimated p99 for {} is higher than the last bucket boundary of {:?} ms",
                    metrics_address, LATENCY_METRIC_PREFIX, last_bucket_boundary
                );
            } else {
                info!(
                    "{} -> Estimated p99 for {}: {:.2} ms",
                    metrics_address, LATENCY_METRIC_PREFIX, p99
                );
            }
            if p99 > PROXY_LATENCY_P99_THRESHOLD {
                if p99 == f64::INFINITY {
                    error!(
                        "Proxy of validator {} unhealthy! Latency p99 is too high, it is higher than \
                        the last bucket boundary of {:.2} ms",
                        metrics_address, last_bucket_boundary
                    );
                } else {
                    error!(
                        "Proxy of validator {} unhealthy! Latency p99 is too high: {:.2} ms",
                        metrics_address, p99
                    );
                }
                return Ok(false);
            }
            previous_histogram_snapshots.insert(metrics_address.clone(), histogram);
        }

        Ok(true)
    }

    fn diff_histograms(
        previous: &HistogramSnapshot,
        current: &HistogramSnapshot,
    ) -> Result<HistogramSnapshot, BenchmarkError> {
        if current.count < previous.count {
            return Err(BenchmarkError::HistogramCountMismatch);
        }
        let total_diff = current.count - previous.count;
        let mut buckets_diff: Vec<HistogramCount> = Vec::new();
        for (before, after) in previous.buckets.iter().zip(current.buckets.iter()) {
            let bound_before = before.less_than;
            let bound_after = after.less_than;
            let cumulative_before = before.count;
            let cumulative_after = after.count;
            if (bound_before - bound_after).abs() > f64::EPSILON {
                return Err(BenchmarkError::BucketBoundariesDoNotMatch(
                    bound_before,
                    bound_after,
                ));
            }
            let diff = (cumulative_after - cumulative_before).max(0.0);
            buckets_diff.push(HistogramCount {
                less_than: bound_after,
                count: diff,
            });
        }
        Ok(HistogramSnapshot {
            buckets: buckets_diff,
            count: total_diff,
            sum: current.sum - previous.sum,
        })
    }

    async fn get_scrapes(
        metrics_addresses: &[String],
    ) -> Result<Vec<(String, Scrape)>, BenchmarkError> {
        let mut scrapes = Vec::new();
        for metrics_address in metrics_addresses {
            let response = reqwest::get(metrics_address)
                .await
                .map_err(BenchmarkError::Reqwest)?;
            let metrics = response.text().await.map_err(BenchmarkError::Reqwest)?;
            let scrape = Scrape::parse(metrics.lines().map(|line| Ok(line.to_owned())))
                .map_err(BenchmarkError::IoError)?;
            scrapes.push((metrics_address.clone(), scrape));
        }
        Ok(scrapes)
    }

    fn parse_histogram(
        scrape: &Scrape,
        metric_prefix: &str,
    ) -> Result<HistogramSnapshot, BenchmarkError> {
        let mut buckets: Vec<HistogramCount> = Vec::new();
        let mut total_count: Option<f64> = None;
        let mut total_sum: Option<f64> = None;

        // Iterate over each metric in the scrape.
        for sample in &scrape.samples {
            if sample.metric == metric_prefix {
                if let Value::Histogram(histogram) = &sample.value {
                    buckets.extend(histogram.iter().cloned());
                } else {
                    return Err(BenchmarkError::ExpectedHistogramValue(sample.value.clone()));
                }
            } else if sample.metric == format!("{}_count", metric_prefix) {
                if let Value::Untyped(count) = sample.value {
                    total_count = Some(count);
                } else {
                    return Err(BenchmarkError::ExpectedUntypedValue(sample.value.clone()));
                }
            } else if sample.metric == format!("{}_sum", metric_prefix) {
                if let Value::Untyped(sum) = sample.value {
                    total_sum = Some(sum);
                } else {
                    return Err(BenchmarkError::ExpectedUntypedValue(sample.value.clone()));
                }
            }
        }

        match (total_count, total_sum) {
            (Some(count), Some(sum)) if !buckets.is_empty() => {
                buckets.sort_by(|a, b| {
                    a.less_than
                        .partial_cmp(&b.less_than)
                        .expect("Comparison should not fail")
                });
                Ok(HistogramSnapshot {
                    buckets,
                    count,
                    sum,
                })
            }
            _ => Err(BenchmarkError::IncompleteHistogramData),
        }
    }

    fn compute_quantile(
        buckets: &[HistogramCount],
        total_count: f64,
        quantile: f64,
    ) -> Result<f64, BenchmarkError> {
        if total_count == 0.0 {
            // Had no samples in the last 5s.
            return Err(BenchmarkError::NoDataYetForP99Calculation);
        }
        // Compute the target cumulative count.
        let target = (quantile * total_count).ceil();
        let mut prev_cumulative = 0.0;
        let mut prev_bound = 0.0;
        for bucket in buckets {
            if bucket.count >= target {
                let bucket_count = bucket.count - prev_cumulative;
                if bucket_count == 0.0 {
                    // Bucket that is supposed to contain the target quantile is empty, unexpectedly.
                    return Err(BenchmarkError::UnexpectedEmptyBucket);
                }
                let fraction = (target - prev_cumulative) / bucket_count;
                return Ok(prev_bound + (bucket.less_than - prev_bound) * fraction);
            }
            prev_cumulative = bucket.count;
            prev_bound = bucket.less_than;
        }
        Err(BenchmarkError::CouldNotComputeQuantile)
    }

    #[expect(clippy::too_many_arguments)]
    async fn run_benchmark_internal(
        signer: AccountOwner,
        bps: Option<usize>,
        operations: Vec<Operation>,
        epoch: Epoch,
        chain_client: ChainClient<Env>,
        shutdown_notifier: CancellationToken,
        sender: crossbeam_channel::Sender<()>,
        committee: Committee,
        bps_tasks_logger_sender: mpsc::Sender<()>,
        barrier: Arc<Barrier>,
    ) -> Result<(), BenchmarkError> {
        let chain_id = chain_client.chain_id();
        bps_tasks_logger_sender.send(()).await?;
        barrier.wait().await;
        info!(
            "Starting benchmark at target BPS of {:?}, for chain {:?}",
            bps, chain_id
        );
        let mut num_sent_proposals = 0;
        loop {
            if shutdown_notifier.is_cancelled() {
                info!("Shutdown signal received, stopping benchmark");
                break;
            }

            chain_client
                .submit_fast_block_proposal(&committee, epoch, &operations, signer)
                .await
                .map_err(BenchmarkError::ChainClient)?;
            // We assume the committee will not change during the benchmark.
            chain_client
                .communicate_chain_updates(&committee)
                .await
                .map_err(BenchmarkError::ChainClient)?;

            num_sent_proposals += 1;
            if let Some(bps) = bps {
                if num_sent_proposals == bps {
                    sender.send(())?;
                    num_sent_proposals = 0;
                }
            } else {
                sender.send(())?;
                break;
            }
        }

        info!("Exiting task...");
        Ok(())
    }

    /// Closes the chain that was created for the benchmark.
    pub async fn close_benchmark_chain(
        chain_client: &ChainClient<Env>,
    ) -> Result<(), BenchmarkError> {
        let start = Instant::now();
        chain_client
            .execute_operation(Operation::system(SystemOperation::CloseChain))
            .await?
            .expect("Close chain operation should not fail!");

        debug!(
            "Closed chain {:?} in {} ms",
            chain_client.chain_id(),
            start.elapsed().as_millis()
        );

        Ok(())
    }

    /// Generates information related to one block per chain, up to `num_chains` blocks.
    pub fn make_benchmark_block_info(
        keys: HashMap<ChainId, AccountOwner>,
        transactions_per_block: usize,
        fungible_application_id: Option<ApplicationId>,
    ) -> Vec<(ChainId, Vec<Operation>, AccountOwner)> {
        let mut blocks_infos = Vec::new();
        let mut previous_chain_id = *keys
            .iter()
            .last()
            .expect("There should be a last element")
            .0;
        let amount = Amount::from(1);
        for (chain_id, owner) in keys {
            let operation = match fungible_application_id {
                Some(application_id) => {
                    Self::fungible_transfer(application_id, previous_chain_id, owner, owner, amount)
                }
                None => Operation::system(SystemOperation::Transfer {
                    owner: AccountOwner::CHAIN,
                    recipient: Recipient::chain(previous_chain_id),
                    amount,
                }),
            };
            let operations = iter::repeat_n(operation, transactions_per_block).collect();
            blocks_infos.push((chain_id, operations, owner));
            previous_chain_id = chain_id;
        }
        blocks_infos
    }

    /// Creates a fungible token transfer operation.
    pub fn fungible_transfer(
        application_id: ApplicationId,
        chain_id: ChainId,
        sender: AccountOwner,
        receiver: AccountOwner,
        amount: Amount,
    ) -> Operation {
        let target_account = fungible::Account {
            chain_id,
            owner: receiver,
        };
        let bytes = bcs::to_bytes(&fungible::Operation::Transfer {
            owner: sender,
            amount,
            target_account,
        })
        .expect("should serialize fungible token operation");
        Operation::User {
            application_id,
            bytes,
        }
    }
}

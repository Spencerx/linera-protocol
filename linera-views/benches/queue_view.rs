// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
#[cfg(with_dynamodb)]
use linera_views::dynamo_db::DynamoDbDatabase;
#[cfg(with_rocksdb)]
use linera_views::rocks_db::RocksDbDatabase;
#[cfg(with_scylladb)]
use linera_views::scylla_db::ScyllaDbDatabase;
use linera_views::{
    bucket_queue_view::BucketQueueView,
    context::ViewContext,
    memory::MemoryDatabase,
    queue_view::QueueView,
    random::{make_deterministic_rng, DeterministicRng},
    store::{ReadableKeyValueStore, TestKeyValueDatabase, WritableKeyValueStore},
    views::{CryptoHashRootView, RootView, View},
};
use rand::Rng;
use tokio::runtime::Runtime;

/// The number of operations
const N_OPERATIONS: usize = 1000;

enum Operations {
    Save,
    DeleteFront,
    PushBack(u8),
}

fn generate_test_case(n_operation: usize, rng: &mut DeterministicRng) -> Vec<Operations> {
    let mut operations = Vec::new();
    let mut total_length = 0;
    for _ in 0..n_operation {
        let choice = rng.gen_range(0..10);
        if choice == 0 {
            operations.push(Operations::Save);
        } else if choice < 3 && total_length > 0 {
            operations.push(Operations::DeleteFront);
            total_length -= 1;
        } else {
            let val = rng.gen::<u8>();
            operations.push(Operations::PushBack(val));
            total_length += 1;
        }
    }
    operations
}

#[derive(CryptoHashRootView)]
pub struct QueueStateView<C> {
    pub queue: QueueView<C, u8>,
}

pub async fn performance_queue_view<D: TestKeyValueDatabase + Clone + 'static>(
    iterations: u64,
) -> Duration
where
    D::Store: ReadableKeyValueStore + WritableKeyValueStore + Clone + 'static,
{
    let database = D::connect_test_namespace().await.unwrap();
    let store = database.open_shared(&[]).unwrap();
    let context = ViewContext::<(), D::Store>::create_root_context(store, ())
        .await
        .unwrap();
    let mut total_time = Duration::ZERO;
    let mut rng = make_deterministic_rng();
    for _ in 0..iterations {
        let operations = generate_test_case(N_OPERATIONS, &mut rng);
        let mut view = QueueStateView::load(context.clone()).await.unwrap();
        let measurement = Instant::now();
        for operation in operations {
            match operation {
                Operations::Save => {
                    view.save().await.unwrap();
                }
                Operations::DeleteFront => {
                    view.queue.delete_front();
                }
                Operations::PushBack(val) => {
                    view.queue.push_back(val);
                }
            }
            black_box(view.queue.front().await.unwrap());
        }
        view.clear();
        view.save().await.unwrap();
        total_time += measurement.elapsed();
    }

    total_time
}

fn bench_queue_view(criterion: &mut Criterion) {
    criterion.bench_function("memory_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_queue_view::<MemoryDatabase>(iterations).await
            })
    });

    #[cfg(with_rocksdb)]
    criterion.bench_function("rocksdb_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_queue_view::<RocksDbDatabase>(iterations).await
            })
    });

    #[cfg(with_dynamodb)]
    criterion.bench_function("dynamodb_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_queue_view::<DynamoDbDatabase>(iterations).await
            })
    });

    #[cfg(with_scylladb)]
    criterion.bench_function("scylladb_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_queue_view::<ScyllaDbDatabase>(iterations).await
            })
    });
}

#[derive(CryptoHashRootView)]
pub struct BucketQueueStateView<C> {
    pub queue: BucketQueueView<C, u8, 100>,
}

pub async fn performance_bucket_queue_view<D: TestKeyValueDatabase + Clone + 'static>(
    iterations: u64,
) -> Duration
where
    D::Store: ReadableKeyValueStore + WritableKeyValueStore + Clone + 'static,
{
    let database = D::connect_test_namespace().await.unwrap();
    let store = database.open_shared(&[]).unwrap();
    let context = ViewContext::<(), D::Store>::create_root_context(store, ())
        .await
        .unwrap();
    let mut total_time = Duration::ZERO;
    let mut rng = make_deterministic_rng();
    for _ in 0..iterations {
        let operations = generate_test_case(N_OPERATIONS, &mut rng);
        let mut view = BucketQueueStateView::load(context.clone()).await.unwrap();
        //
        let measurement = Instant::now();
        for operation in operations {
            match operation {
                Operations::Save => {
                    view.save().await.unwrap();
                }
                Operations::DeleteFront => {
                    view.queue.delete_front().await.unwrap();
                }
                Operations::PushBack(val) => {
                    view.queue.push_back(val);
                }
            }
            black_box(view.queue.front());
        }
        view.clear();
        view.save().await.unwrap();
        total_time += measurement.elapsed();
    }

    total_time
}

fn bench_bucket_queue_view(criterion: &mut Criterion) {
    criterion.bench_function("memory_bucket_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_bucket_queue_view::<MemoryDatabase>(iterations).await
            })
    });

    #[cfg(with_rocksdb)]
    criterion.bench_function("rocksdb_bucket_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_bucket_queue_view::<RocksDbDatabase>(iterations).await
            })
    });

    #[cfg(with_dynamodb)]
    criterion.bench_function("dynamodb_bucket_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_bucket_queue_view::<DynamoDbDatabase>(iterations).await
            })
    });

    #[cfg(with_scylladb)]
    criterion.bench_function("scylladb_bucket_queue_view", |bencher| {
        bencher
            .to_async(Runtime::new().expect("Failed to create Tokio runtime"))
            .iter_custom(|iterations| async move {
                performance_bucket_queue_view::<ScyllaDbDatabase>(iterations).await
            })
    });
}

criterion_group!(benches, bench_queue_view, bench_bucket_queue_view);
criterion_main!(benches);

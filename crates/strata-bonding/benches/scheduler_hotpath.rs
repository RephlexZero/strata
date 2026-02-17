//! Per-packet scheduling latency benchmarks for strata-bonding.
//!
//! Measures the overhead of the bonding scheduler intelligence pipeline:
//! - BondingScheduler.send() with 2 links (DWRR + BLEST + IoDS + Thompson)
//! - BondingScheduler.send() with 3 heterogeneous links
//! - refresh_metrics() cost
//! - Critical broadcast vs standard send
//!
//! Run with: cargo bench --package strata-bonding

use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::sync::{Arc, Mutex};

use strata_bonding::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use strata_bonding::scheduler::bonding::BondingScheduler;
use strata_bonding::scheduler::PacketProfile;

struct MockLink {
    id: usize,
    metrics: Mutex<LinkMetrics>,
}

impl MockLink {
    fn new(id: usize, capacity_bps: f64, rtt_ms: f64) -> Self {
        Self {
            id,
            metrics: Mutex::new(LinkMetrics {
                capacity_bps,
                rtt_ms,
                loss_rate: 0.0,
                observed_bps: 0.0,
                observed_bytes: 0,
                queue_depth: 0,
                max_queue: 100,
                alive: true,
                phase: LinkPhase::Live,
                os_up: Some(true),
                mtu: None,
                iface: None,
                link_kind: None,
            }),
        }
    }
}

impl LinkSender for MockLink {
    fn id(&self) -> usize {
        self.id
    }
    fn send(&self, _packet: &[u8]) -> anyhow::Result<usize> {
        Ok(0)
    }
    fn get_metrics(&self) -> LinkMetrics {
        self.metrics.lock().unwrap().clone()
    }
}

fn bench_scheduler_send_2_links(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_send_2links");

    let mut scheduler = BondingScheduler::new();
    let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
    let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 20.0));
    scheduler.add_link(l1);
    scheduler.add_link(l2);
    scheduler.refresh_metrics();

    for size in [100, 1200] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("{size}B_droppable"), |b| {
            let profile = PacketProfile {
                is_critical: false,
                can_drop: true,
                size_bytes: size,
            };
            b.iter(|| {
                let payload = Bytes::from(vec![0u8; size]);
                black_box(scheduler.send(payload, profile).unwrap());
            });
        });
    }

    group.finish();
}

fn bench_scheduler_send_3_links_hetero(c: &mut Criterion) {
    let mut scheduler = BondingScheduler::new();
    let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
    let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 40.0));
    let l3 = Arc::new(MockLink::new(3, 2_000_000.0, 80.0));
    scheduler.add_link(l1);
    scheduler.add_link(l2);
    scheduler.add_link(l3);
    scheduler.refresh_metrics();

    c.bench_function("scheduler_send_3links_1200B", |b| {
        let profile = PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 1200,
        };
        b.iter(|| {
            let payload = Bytes::from(vec![0u8; 1200]);
            black_box(scheduler.send(payload, profile).unwrap());
        });
    });
}

fn bench_scheduler_refresh_metrics(c: &mut Criterion) {
    let mut scheduler = BondingScheduler::new();
    let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
    let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 20.0));
    let l3 = Arc::new(MockLink::new(3, 2_000_000.0, 40.0));
    scheduler.add_link(l1);
    scheduler.add_link(l2);
    scheduler.add_link(l3);

    c.bench_function("scheduler_refresh_metrics_3links", |b| {
        b.iter(|| {
            scheduler.refresh_metrics();
            black_box(());
        });
    });
}

fn bench_scheduler_critical_broadcast(c: &mut Criterion) {
    let mut scheduler = BondingScheduler::new();
    let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
    let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 20.0));
    let l3 = Arc::new(MockLink::new(3, 2_000_000.0, 40.0));
    scheduler.add_link(l1);
    scheduler.add_link(l2);
    scheduler.add_link(l3);
    scheduler.refresh_metrics();

    c.bench_function("scheduler_critical_broadcast_3links", |b| {
        let profile = PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: 1200,
        };
        b.iter(|| {
            let payload = Bytes::from(vec![0u8; 1200]);
            black_box(scheduler.send(payload, profile).unwrap());
        });
    });
}

criterion_group!(
    benches,
    bench_scheduler_send_2_links,
    bench_scheduler_send_3_links_hetero,
    bench_scheduler_refresh_metrics,
    bench_scheduler_critical_broadcast,
);
criterion_main!(benches);

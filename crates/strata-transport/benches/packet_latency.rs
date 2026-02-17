use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use strata_transport::pool::Priority;
use strata_transport::receiver::{Receiver, ReceiverConfig};
use strata_transport::sender::{Sender, SenderConfig};

/// Benchmark the sender hot path: send() + drain_output().
fn bench_sender_send(c: &mut Criterion) {
    let payload = Bytes::from(vec![0xABu8; 1200]);

    let mut group = c.benchmark_group("sender");
    group.throughput(Throughput::Elements(1));

    group.bench_function("send_single_packet", |b| {
        let mut sender = Sender::new(SenderConfig::default());
        b.iter(|| {
            sender.send(black_box(payload.clone()), Priority::Standard);
            sender.drain_output().for_each(drop);
        });
    });

    group.bench_function("send_100_packets", |b| {
        b.iter(|| {
            let mut sender = Sender::new(SenderConfig::default());
            for _ in 0..100 {
                sender.send(black_box(payload.clone()), Priority::Standard);
            }
            sender.drain_output().for_each(drop);
        });
    });

    group.finish();
}

/// Benchmark the receiver hot path: receive() + drain_events().
fn bench_receiver_receive(c: &mut Criterion) {
    // Pre-encode packets via a sender so we have valid wire bytes
    let payload = Bytes::from(vec![0xABu8; 1200]);
    let mut sender = Sender::new(SenderConfig::default());

    let mut wire_packets = Vec::new();
    for _ in 0..200 {
        sender.send(payload.clone(), Priority::Standard);
    }
    for out in sender.drain_output() {
        if !out.is_fec_repair {
            wire_packets.push(out.data);
        }
    }

    let mut group = c.benchmark_group("receiver");
    group.throughput(Throughput::Elements(1));

    group.bench_function("receive_single_packet", |b| {
        let mut idx = 0;
        let mut receiver = Receiver::new(ReceiverConfig::default());
        b.iter(|| {
            let pkt = &wire_packets[idx % wire_packets.len()];
            receiver.receive(black_box(pkt.clone()));
            receiver.drain_events().for_each(drop);
            idx += 1;
        });
    });

    group.finish();
}

/// Benchmark full send→receive round-trip (in-process, no network).
fn bench_send_receive_roundtrip(c: &mut Criterion) {
    let payload = Bytes::from(vec![0xABu8; 1200]);

    let mut group = c.benchmark_group("roundtrip");
    group.throughput(Throughput::Elements(1));

    group.bench_function("send_then_receive", |b| {
        let mut sender = Sender::new(SenderConfig::default());
        let mut receiver = Receiver::new(ReceiverConfig::default());
        b.iter(|| {
            sender.send(black_box(payload.clone()), Priority::Standard);
            for out in sender.drain_output() {
                receiver.receive(out.data);
            }
            for event in receiver.drain_events() {
                black_box(event);
            }
        });
    });

    group.bench_function("send_then_receive_with_fec", |b| {
        let mut sender = Sender::new(SenderConfig::default()); // K=32, R=4
        let mut receiver = Receiver::new(ReceiverConfig::default());
        b.iter(|| {
            sender.send(black_box(payload.clone()), Priority::Standard);
            for out in sender.drain_output() {
                receiver.receive(out.data);
            }
            for event in receiver.drain_events() {
                black_box(event);
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_sender_send,
    bench_receiver_receive,
    bench_send_receive_roundtrip
);
criterion_main!(benches);

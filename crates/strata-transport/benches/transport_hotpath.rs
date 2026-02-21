//! Per-packet processing latency benchmarks for strata-transport.
//!
//! Measures latency contributions of the hot-path components:
//! - VarInt encode/decode
//! - PacketHeader encode/decode
//! - Full Packet encode/decode (various payload sizes)
//! - FEC encoder (add_source_symbol + generation completion)
//! - Sender.send() (the full send pipeline: fragment + FEC + queue)
//!
//! Run with: cargo bench --package strata-transport

use bytes::{Bytes, BytesMut};
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::time::Duration;

use strata_transport::codec::FecEncoder;
use strata_transport::pool::Priority;
use strata_transport::sender::{Sender, SenderConfig};
use strata_transport::wire::{Packet, PacketHeader, VarInt};

// ─── VarInt ──────────────────────────────────────────────────────────────

fn bench_varint_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("varint_encode");

    // 1-byte encoding (0..63)
    group.bench_function("1byte", |b| {
        let vi = VarInt::from_u64(42);
        b.iter(|| {
            let mut buf = BytesMut::with_capacity(8);
            black_box(vi).encode(&mut buf);
            black_box(buf);
        });
    });

    // 2-byte encoding (64..16383)
    group.bench_function("2byte", |b| {
        let vi = VarInt::from_u64(1000);
        b.iter(|| {
            let mut buf = BytesMut::with_capacity(8);
            black_box(vi).encode(&mut buf);
            black_box(buf);
        });
    });

    // 4-byte encoding (16384..2^30-1)
    group.bench_function("4byte", |b| {
        let vi = VarInt::from_u64(100_000);
        b.iter(|| {
            let mut buf = BytesMut::with_capacity(8);
            black_box(vi).encode(&mut buf);
            black_box(buf);
        });
    });

    // 8-byte encoding (2^30..)
    group.bench_function("8byte", |b| {
        let vi = VarInt::from_u64(2_000_000_000);
        b.iter(|| {
            let mut buf = BytesMut::with_capacity(8);
            black_box(vi).encode(&mut buf);
            black_box(buf);
        });
    });

    group.finish();
}

fn bench_varint_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("varint_decode");

    for (name, val) in [
        ("1byte", 42u64),
        ("2byte", 1000),
        ("4byte", 100_000),
        ("8byte", 2_000_000_000),
    ] {
        let vi = VarInt::from_u64(val);
        let mut buf = BytesMut::with_capacity(8);
        vi.encode(&mut buf);
        let encoded = buf.freeze();

        group.bench_function(name, |b| {
            b.iter(|| {
                let mut r = encoded.clone();
                black_box(VarInt::decode(&mut r));
            });
        });
    }

    group.finish();
}

// ─── PacketHeader ────────────────────────────────────────────────────────

fn bench_header_encode(c: &mut Criterion) {
    let hdr = PacketHeader::data(42, 1_000_000, 1200);
    c.bench_function("header_encode", |b| {
        b.iter(|| {
            let mut buf = BytesMut::with_capacity(16);
            black_box(&hdr).encode(&mut buf);
            black_box(buf);
        });
    });
}

fn bench_header_decode(c: &mut Criterion) {
    let hdr = PacketHeader::data(42, 1_000_000, 1200);
    let mut buf = BytesMut::with_capacity(16);
    hdr.encode(&mut buf);
    let encoded = buf.freeze();

    c.bench_function("header_decode", |b| {
        b.iter(|| {
            let mut r = encoded.clone();
            black_box(PacketHeader::decode(&mut r));
        });
    });
}

// ─── Full Packet ─────────────────────────────────────────────────────────

fn bench_packet_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("packet_encode");

    for size in [100, 500, 1200, 4000] {
        let pkt = Packet::new_data(100, 42_000, Bytes::from(vec![0xAB; size]));
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("{size}B"), |b| {
            b.iter(|| {
                black_box(black_box(&pkt).encode());
            });
        });
    }

    group.finish();
}

fn bench_packet_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("packet_decode");

    for size in [100, 500, 1200, 4000] {
        let pkt = Packet::new_data(100, 42_000, Bytes::from(vec![0xAB; size]));
        let encoded = pkt.encode().freeze();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("{size}B"), |b| {
            b.iter(|| {
                let mut r = encoded.clone();
                black_box(Packet::decode(&mut r));
            });
        });
    }

    group.finish();
}

// ─── FEC Encoder ─────────────────────────────────────────────────────────

fn bench_fec_add_source(c: &mut Criterion) {
    c.bench_function("fec_add_source_noncomplete", |b| {
        let mut enc = FecEncoder::new(32, 4);
        let mut seq = 0u64;
        b.iter(|| {
            let payload = Bytes::from(vec![seq as u8; 1200]);
            let repairs = enc.add_source_symbol(seq, payload);
            if !repairs.is_empty() {
                black_box(repairs);
            }
            seq += 1;
        });
    });
}

fn bench_fec_generation_complete(c: &mut Criterion) {
    c.bench_function("fec_generation_complete_k32_r4", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut enc = FecEncoder::new(32, 4);
            let mut seq = 0u64;

            for _ in 0..iters {
                // Fill 31 symbols (not benchmarked)
                for _ in 0..31 {
                    let _ = enc.add_source_symbol(seq, Bytes::from(vec![seq as u8; 1200]));
                    seq += 1;
                }
                // Benchmark the 32nd (completing) symbol
                let start = quanta::Instant::now();
                let repairs = enc.add_source_symbol(seq, Bytes::from(vec![seq as u8; 1200]));
                total += start.elapsed();
                black_box(repairs);
                seq += 1;
            }

            total
        });
    });
}

// ─── Sender Pipeline ─────────────────────────────────────────────────────

fn bench_sender_send(c: &mut Criterion) {
    let mut group = c.benchmark_group("sender_send");

    for size in [100, 1200, 4000] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("{size}B"), |b| {
            let config = SenderConfig {
                max_payload_size: 1200,
                pool_capacity: 8192,
                fec_k: 32,
                fec_r: 4,
                packet_ttl: Duration::from_secs(5),
                max_retries: 3,
            };
            let mut sender = Sender::new(config);

            b.iter(|| {
                let payload = Bytes::from(vec![0xAB; size]);
                let n = sender.send(payload, Priority::Standard);
                black_box(n);
                // Drain output to prevent unbounded queue growth
                sender.drain_output().for_each(|p| {
                    black_box(p);
                });
            });
        });
    }

    group.finish();
}

fn bench_sender_drain(c: &mut Criterion) {
    c.bench_function("sender_drain_32_packets", |b| {
        let config = SenderConfig::default();
        let mut sender = Sender::new(config);

        b.iter(|| {
            for i in 0..32u8 {
                sender.send(Bytes::from(vec![i; 1200]), Priority::Standard);
            }
            let count: usize = sender
                .drain_output()
                .map(|p| {
                    black_box(p);
                    1
                })
                .sum();
            black_box(count);
        });
    });
}

// ─── Roundtrip ───────────────────────────────────────────────────────────

fn bench_packet_roundtrip(c: &mut Criterion) {
    c.bench_function("packet_roundtrip_1200B", |b| {
        b.iter(|| {
            let pkt = Packet::new_data(100, 42_000, Bytes::from(vec![0xAB; 1200]));
            let encoded = pkt.encode();
            let decoded = Packet::decode(&mut encoded.freeze());
            black_box(decoded);
        });
    });
}

criterion_group!(
    benches,
    bench_varint_encode,
    bench_varint_decode,
    bench_header_encode,
    bench_header_decode,
    bench_packet_encode,
    bench_packet_decode,
    bench_fec_add_source,
    bench_fec_generation_complete,
    bench_sender_send,
    bench_sender_drain,
    bench_packet_roundtrip,
);
criterion_main!(benches);

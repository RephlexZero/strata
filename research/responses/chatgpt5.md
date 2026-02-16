Async Runtime Choice
For a moderate number (3–6) of 2–10 Mbps UDP links, both async and threaded models are viable. The
Tokio ecosystem (e.g. tokio = "1.x" ) is the mainstream choice: it provides mature UDP I/O and
timer support (via tokio::net::UdpSocket , tokio::time ) and a multi-threaded scheduler.
Benchmarks (ping-pong style tests) show Tokio has slightly higher per-call overhead (on the order of
~8 µs extra latency) than lean runtimes . By contrast, lightweight runtimes like smol (v2.0.2) or
async-std (final v1.13.2, now effectively superseded by smol ) incur less overhead and can match
std performance. For example, a Zenoh blog reports that smol/async-std outperform the standard
library and significantly outperform Tokio in small-message benchmarks .
Specialized runtimes use a thread-per-core model. glommio (v0.9.0) and monoio (v0.2.4) pin tasks to
dedicated cores and leverage Linux’s io_uring for very high throughput. They require Linux 5.6+ and are
more complex to integrate, but can deliver lower latency and higher throughput in CPU-bound
scenarios. If you have full control of the OS (and can upgrade the kernel and set the required mlock
limits for glommio), these are worth considering for maximum throughput. Otherwise, standard Tokio
or smol on a threaded runtime should suffice. In fact, for only a few thousand packets per second, even
a thread-per-link model with blocking I/O (each thread doing recv_from / send_to on its socket)
plus coordination via crossbeam channels will work well; the overhead of a full async runtime may
not be needed. In practice, tasks like parallel packet processing and timers can be handled in either
model. Tokio’s multi-threaded executor easily handles timers (100 ms intervals, NACK scheduling, etc.),
and Tokio/async-std/smol all support high-resolution sleep .
Recommendation: Tokio (1.x) or smol (2.0.2) are safe defaults (with stable APIs and large ecosystem).
For extreme latency/performance, glommio (0.9.0) or monoio (0.2.4) provide thread-per-core async with
io_uring, but at the cost of complexity and Linux-only support. Given ~3k pps, even a simple threaded
design is acceptable; choose async if you prefer composability. (Note: async-std is effectively
discontinued .)
Buffer Management (Zero-Copy)
Packets (≈1.5 KB each) live ~1 s in a retransmit buffer (~3000 packets total). Common strategies: using
bytes::Bytes (from the Tokio bytes crate), which provides cheap slicing and reference-counting;
or a custom memory pool/allocator (slab or arena) to avoid per-packet allocation. Bytes allows zerocopy sharing of buffers: you can wrap a received Vec<u8> in a BytesMut and call .freeze() , or
clone slices, without copying data. However, each clone uses an atomic refcount, which has a small cost.
Alternatively, a fixed-size slab allocator or object pool can recycle packet buffers. For example, the
sharded-slab crate provides a lock-free slab/pool for reuse. Benchmarks indicate that a sharded slab
gives “significantly improved performance in concurrent use-cases” compared to a locked slab ,
reducing allocator overhead and fragmentation. In a single-threaded context, a simple Vec or Slab
is fine; in multi-thread use the concurrent sharded-slab (v0.1.7) or crossbeam’s lock-free structures.
The slab/pool approach retains heap allocations to reuse them, which can be more cache-friendly than
many one-off allocations.
1
2
1
2
3
1
Quic libraries: Implementation details vary. For example, AWS’s s2n-quic uses a custom segment ring
buffer for stream data, and Quinn’s internals use pre-allocated frame buffers (often Bytes ). In
practice, for only ~3000 pps (~1.5 MB/s), either approach is fine. bytes::Bytes (Tokio’s buffers) is
simplest: use a BytesMut per packet and push clones into your retransmit map. If you want to avoid
atomic refcounts, use a pool: e.g. the sharded-slab crate’s Pool type (v0.1.x) lets you recycle equalsized buffers . For maximum cache-friendliness, you could pre-allocate one large BytesMut and
slice it (similar to a ring buffer), but that adds complexity. In summary, bytes::Bytes is easy and
usually performant; if profiling shows allocator overhead, switch to a slab or bump-pool strategy.
Scheduling Data Structures
Our DWRR scheduler (≈3000/s) needs to queue and dequeue packets efficiently. This is low volume:
even naive locks would cost only tens of µs total. However, lock-free queues and maps are available:
Queues: For inter-thread message passing, crossbeam::SegQueue (unbounded MPMC) or
crossbeam-channel (synchronous or asynchronous channels) are popular. For single-producer
single-consumer (SPSC), a ring buffer like rtrb (v0.3.2) is ultra-fast and wait-free . In fact, SPSC
ring buffers (e.g. [rtrb]) can achieve ~1 ns per operation in benchmarks, whereas crossbeamchannel SPSC is ~20 ns . At 3k msgs/s, either is trivial. If you require a bounded queue with no
allocation, rtrb (or ArrayQueue from crossbeam) is ideal.
Hash maps: For storing per-packet state (e.g. NACK sets keyed by sequence), DashMap (v6.x) is
a fast sharded concurrent hashmap (uses many shard-level locks) and is very popular. According
to its author, DashMap “outperforms the port of Java’s ConcurrentHashMap (flurry) by a
significant amount” . The Flurry crate (v0.5.x) is a port of Java’s CHM; it offers lock-free reads
but uses crossbeam epoch GC and tends to be slower under high insert/delete load. In practice,
at ~3k/s, either works. If contention is low, a standard RwLock<HashMap> or single-threaded
HashMap with message passing might suffice.
Alternative: Given the moderate throughput, a single-threaded scheduler can be simplest. For
example, have one thread own the DWRR logic and receive packets via channels (or tokio tasks).
This removes locks entirely. Many high-performance systems do exactly this (actor/event loop
per core). If even faster scheduling is needed, one can use OS-assisted batching: e.g. Linux’s
io_uring can batch UDP send submissions (using sendmmsg ) which amortizes locks/syscalls
across packets. However, at 3 kHz this is likely unnecessary complexity.
Summary: Use crossbeam queues or channels for inter-thread work (both are lock-free and <20 ns/op
). For maps, DashMap (v6.x) is easy and fast ; Flurry (v0.5.2) is an alternative. Or avoid shared
state altogether by serializing scheduling in one thread.
High-Precision Timing
For RTT/jitter calculations we need µs precision. On modern Linux, std::time::Instant::now() is
nanosecond-resolution and monotonic, so it can easily give microsecond precision. In fact, on Linux
Rust’s Instant internally calls clock_gettime(CLOCK_MONOTONIC) . This is typically a
nanosecond-precision hardware clock (NTP-slewed, but without jumps) – StackOverflow confirms that
Instant::now() matches clock_gettime(CLOCK_MONOTONIC) on Unix . In practice, its
3
•
4
5
•
6
•
5 6
7
7
2
resolution is sub-microsecond (often ~20–40 ns on a 3 GHz CPU as shown in tests ), so it is more than
adequate for µs timings.
If you want raw hardware time (immune to NTP slewing), use
clock_gettime(CLOCK_MONOTONIC_RAW) via libc – but Rust’s Instant doesn’t expose this directly.
CLOCK_MONOTONIC (used by Instant) will not jump backward, but it can be slewed (frequency adjusted)
by NTP . For pure measurement (no system-time synchrony), RAW avoids that. Alternatively, one can
use the quanta crate (v0.12.6) which offers very fast clocks: it uses the CPU’s TSC if available, falling back
to the OS clock . TSC (Time Stamp Counter) can be read extremely quickly, and quanta calibrates it to
real time. Note: TSC must be invariant and synchronized across cores to be reliable. On modern ARM
(RPI5, Jetson), there is an equivalent cycle counter (e.g. ARM’s generic timer or PMU counters) – libraries
like quanta handle architecture differences.
TSC caveats: The x86 TSC historically could drift or be unsynchronized across cores unless “invariant
TSC” is supported. Wikipedia notes that without care, multiple CPUs’ TSCs may not stay in sync and can
vary with power states . With modern hardware and kernel (constant-rate TSC), these issues are
largely solved . Still, a safe option is to use clock_gettime or quanta’s abstraction. In summary,
std::time::Instant should suffice (native Linux monotonic clock, ~ns resolution ). For fastest
calls or guaranteed monotonicity, consider the quanta crate’s TSC-based Clock .
FEC (RaptorQ / Reed-Solomon)
Rust has multiple FEC crates: for fountain codes, there’s the raptorq crate (RFC 6330). Christopher
Berner’s implementation ( raptorq ) has been optimized to over 100 Mb/s encode/decode on modern
hardware. For example, version 0.3.0 of his RaptorQ library reached ~110 Mbit/s encoding throughput
. On a Raspberry Pi 3 B+ (Cortex-A53 @1.4GHz), benchmarks show encoding 127 MB in ~3.97 s
(≈258 Mbit/s) and decoding in ~4.94 s (≈207 Mbit/s) with 1280-byte symbols . Even with 5%
overhead (extra symbols), it still did ~200 Mbit/s on Pi3 . These throughputs far exceed our needs
(total ~10–60 Mbit/s), so RaptorQ should be CPU-feasible on an RPi5 or Jetson. The crate on crates.io is
likely around v0.4 (check latest on crates.io for “raptorq”).
For Reed–Solomon FEC, the reed-solomon-erasure crate (v6.x on crates.io) by Jake Ross is a popular
choice. It does block erasure coding with CPU-backed GF(2^8) arithmetic (and SIMD optimizations).
There’s also the reed-solomon-simd family (based on Christopher Taylor’s Leopard-RS FFT approach)
which provides O(n log n) performance for large shard counts. Benchmarks aren’t quoted here, but
these implementations typically saturate memory bandwidth on modern CPUs, far above 10 Mb/s. For
example, if RaptorQ hits ~200 Mb/s on Pi3, simpler RS for a few percent overhead should be even faster
per byte. In practice, encoding a few packets at ~1–2 KB each is trivial.
ARM performance: The Pi3 benchmarks above suggest ~200 Mb/s on A53@1.4GHz. A Pi5 (Cortex-A76
@2.3GHz) or Jetson’s ARM64 CPUs will be significantly faster. Thus FEC CPU use (even at 5–10%
redundancy) is easily within budget. For small coding windows, CPU overhead is low. Benchmarks: see
 for RaptorQ data. (No direct Reed–Solomon bench cited, but similar logic applies.)
7
8
9
10
11
7
12
13 14
15
12 13
3
Testing Strategies (Proptest, Simulation,
Fuzzing)
Property-based tests: Rust’s proptest (v1.0.0) and quickcheck (v1.x) are common. proptest is
more flexible (explicit strategies) and often preferred. Use them to check invariants over sequences of
packets (random drop/reorder patterns, large sequence numbers to test wrap-around, etc.). For
example, generate random loss patterns and assert eventual recovery. There are no formal published
benchmarks here, but many Rust projects use proptest.
Deterministic simulation (DST): Inspired by systems like FoundationDB, one can simulate the entire
protocol in a single-threaded environment with a seeded RNG. The S2 blog on DST (2025) explains this
approach. It describes using Tokio’s single-threaded runtime with a controlled clock (no real sleeps) and
using a framework like Turmoil to simulate network hosts and inject latency/failure . Turmoil
provides Tokio-compatible sockets whose behavior (delay, drop, reorder) is driven by a seeded RNG – so
each test run (seed) reproducibly exercises a random scenario. The author also mentions MadSim/MadTurmoil for overriding libc time and entropy to ensure full determinism . In practice, you could run
your protocol stack under such a simulator to exhaustively test wraparound, reordering, losses, or
duplicate packets.
Fuzz testing: Use crates like cargo-fuzz (libFuzzer) or afl.rs to fuzz packet streams. With cargo-fuzz,
write a fuzz target that feeds random (or mutated real) UDP packet sequences into your protocol
handler, looking for panics or invariant violations. AFL (via the afl crate or LibAFL) is another option.
These tools will automatically generate network scenarios (random bytes, random reorder/delay) to try
to break the protocol logic. They are well-suited to finding edge cases like sequence wrap-around or
buffer overflows.
Example tools:
- Proptest (v1.0) / QuickCheck (v1.0) for property tests.
- tokio-timer tests and [mio-timer] for deterministic timers.
- Turmoil and MadSim (used in DST) for simulation.
- cargo-fuzz (LLVM libFuzzer) – latest is just “cargo-fuzz” subcommand.
- afl.rs or libafl for AFL-style fuzzing.
By combining these: e.g., use simulation tests to sweep through timed scenarios deterministically, use
proptest to cover random losses/wrap, and use fuzzers to find corner-case panics, you can achieve
exhaustive coverage of reorder/loss/wrap scenarios. Each of these techniques can be run in CI.
Citations: The above draws on Rust performance blogs and docs: e.g., async-runtime benchmarks ,
lock-free slab docs , queue latency quotes , DashMap/Flurry HN discussion , clock precision
notes , RaptorQ benchmarks , and deterministic testing frameworks . Each should be
checked against your final constraints (e.g. kernel versions, CPU features) to ensure compatibility.
A Performance Evaluation on Rust Asynchronous Frameworks · Zenoh - pub/sub, geo distributed
storage, query
https://zenoh.io/blog/2022-04-14-rust-async-eval/
async-std 1.13.2 - Docs.rs
https://docs.rs/crate/async-std/latest
16 17
18
1
3 5 6
7 8 12 13 17
1
2
4
sharded_slab - Rust
https://docs.rs/sharded-slab/latest/sharded_slab/
rtrb - Rust
https://docs.rs/rtrb/latest/rtrb/
Low latency queues in Rust ecosystem : r/rust
https://www.reddit.com/r/rust/comments/14pi4n0/low_latency_queues_in_rust_ecosystem/
Dashmap: Fast concurrent HashMap for Rust | Hacker News
https://news.ycombinator.com/item?id=22699176
rust - Is std::time::Duration as precise as time::precise_time_ns from "time" crate? - Stack Overflow
https://stackoverflow.com/questions/55583503/is-stdtimeduration-as-precise-as-timeprecise-time-ns-from-time-crate
linux - What is the difference between CLOCK_MONOTONIC & CLOCK_MONOTONIC_RAW? - Stack
Overflow
https://stackoverflow.com/questions/14270300/what-is-the-difference-between-clock-monotonic-clock-monotonic-raw
quanta - Rust
https://docs.rs/quanta/latest/quanta/
Time Stamp Counter - Wikipedia
https://en.wikipedia.org/wiki/Time_Stamp_Counter
RaptorQ (RFC6330) and performance optimization in Rust
https://www.cberner.com/2019/03/30/raptorq-rfc6330-rust-optimization/
GitHub - cberner/raptorq: Rust implementation of RaptorQ (RFC6330)
https://github.com/cberner/raptorq
Deterministic simulation testing for async Rust
https://s2.dev/blog/dst
3
4
5
6
7
8
9
10 11
12
13 14 15
16 17 18
5
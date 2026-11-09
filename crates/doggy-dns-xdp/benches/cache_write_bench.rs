#![allow(clippy::disallowed_methods)]

//! Contended-write benchmark for the XDP DNS cache map.
//!
//! Two shapes are measured over 1, 2 and 4 threads, each writing pre-built
//! distinct entries into the same kernel map:
//!
//! - `a_mutex`: one [`tokio::sync::Mutex`] around one map handle, acquired per
//!   write — the shape the middleware shipped before the lock removal.
//! - `b_per_thread`: one duplicated map handle per thread (a second descriptor
//!   on the same kernel map), used through a thread-local with no lock — the
//!   shape the middleware ships now.
//!
//! The multi-thread speedup of `b_per_thread` is measured against the pre-change
//! shape's parking: this sync bench takes the same [`tokio::sync::Mutex`] through
//! `blocking_lock()`, which parks the caller the way the production `lock().await`
//! did. A cited off-tree [`std::sync::Mutex`] cross-check — a one-off, not part of
//! this file — bounds the same comparison at roughly 1.8x/3.4x for 2/4 threads, so
//! the 48x-66x is a parking artifact, not headroom in the syscall. The per-thread
//! shape still wins materially under contention.
//!
//! The eBPF ELF is loaded here directly rather than through `XdpManager`, so
//! the benchmark needs no network interface and no attach: maps only, at a
//! pin path unique to this process. It needs Linux and root (bpffs writes);
//! when setup fails the reason is printed and nothing is measured, so CI
//! without those stays green.
//!
//! Run with: cargo bench -p doggy-dns-xdp --bench cache_write_bench

#[cfg(target_os = "linux")]
mod linux_bench {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use anyhow::{Context as _, Result};
    use criterion::{Criterion, Throughput};
    use doggy_dns_xdp::cache::DnsCacheMap;
    use doggy_dns_xdp::middleware::compute_cache_entry;
    use doggy_dns_xdp::types::{CacheKey, CacheValue};
    use hickory_proto::op::ResponseCode;
    use hickory_proto::rr::Name;
    use tokio::sync::Mutex;

    /// Thread counts the gate compares. The server's runtime is built from
    /// `worker_threads`, which configures 1..4.
    const THREAD_COUNTS: [usize; 3] = [1, 2, 4];

    /// Writes per thread per measured batch. Large enough that two barrier
    /// synchronisations per batch (a few microseconds) do not show up against
    /// the syscalls, small enough that one batch stays in the millisecond
    /// range.
    const INSERTS_PER_THREAD: usize = 2048;

    /// Keys each shape needs: one run per thread count, every run over its own
    /// keys, so no measurement ever re-updates an entry an earlier one wrote.
    const KEYS_PER_SHAPE: usize =
        (THREAD_COUNTS[0] + THREAD_COUNTS[1] + THREAD_COUNTS[2]) * INSERTS_PER_THREAD;

    /// Entries must not be evicted mid-measurement: eviction cost is not what
    /// is being compared. Two shapes of `KEYS_PER_SHAPE` keys each plus a probe
    /// key fit with room to spare.
    const MAP_ENTRIES: u32 = 1 << 16;

    /// One pinned map, plus one handle per shape: shape A writes through a
    /// handle taken by a second loader run that reuses the same pin, shape B
    /// duplicates the first handle's descriptor. Both land on one kernel map,
    /// which `setup` asserts before measuring.
    struct Setup {
        header: DnsCacheMap,
        mutex_shape: DnsCacheMap,
        pin: PathBuf,
        _ebpf: Vec<aya::Ebpf>,
    }

    fn pin_path() -> PathBuf {
        PathBuf::from(format!(
            "/sys/fs/bpf/doggy-dns-bench-{}",
            std::process::id()
        ))
    }

    /// Load the embedded ELF against one pin path and take `DNS_CACHE`.
    fn load_cache(pin: &Path) -> Result<(aya::Ebpf, DnsCacheMap)> {
        let elf_bytes = include_bytes!(env!("XDP_EBPF_ELF"));
        anyhow::ensure!(
            !elf_bytes.is_empty(),
            "the embedded eBPF ELF is empty; build it first"
        );
        let mut ebpf = aya::EbpfLoader::new()
            .map_max_entries("DNS_CACHE", MAP_ENTRIES)
            // The loader stores an owned `Cow<Path>`; the comment in
            // `program.rs` records why a `String` is not accepted here.
            .map_pin_path("DNS_CACHE", pin.to_path_buf())
            .load(elf_bytes)
            .context("load the eBPF ELF")?;
        let map = ebpf
            .take_map("DNS_CACHE")
            .context("DNS_CACHE map not found in ELF")?;
        let cache = DnsCacheMap::from_map(map).context("wrap DNS_CACHE")?;
        Ok((ebpf, cache))
    }

    fn setup() -> Result<Setup> {
        let pin = pin_path();
        // A previous run killed before cleanup leaves the pin behind, and the
        // second load below would reuse whatever capacity that map has.
        let _ = std::fs::remove_file(&pin);
        let (first, mut header) = load_cache(&pin)?;
        // The second load opens the pin path the first one created, which is
        // the reuse path a restart takes: same map, independent handle.
        let (second, mut mutex_shape) = load_cache(&pin)?;

        // Prove the two handles really are one map before measuring either:
        // the comparison is only meaningful if both shapes write to it.
        let (key, value) = entry("handles-agree");
        mutex_shape.insert(&key, &value).context("probe insert")?;
        anyhow::ensure!(
            header.get(&key)?.is_some(),
            "the two loader runs did not land on one map"
        );

        Ok(Setup {
            header,
            mutex_shape,
            pin,
            _ebpf: vec![first, second],
        })
    }

    /// One pre-built positive entry for a name unique to `tag`.
    fn entry(tag: &str) -> (CacheKey, CacheValue) {
        let name = Name::from_ascii(format!("{tag}.bench.example.")).expect("valid bench name");
        compute_cache_entry(&name, 1, 1, ResponseCode::NoError, &[], Some(60), 30, 0)
    }

    /// Distinct entries for one shape, dished out run by run: each thread of
    /// each measurement writes keys no other thread and no other run touches.
    fn entries(tag: &str) -> Vec<(CacheKey, CacheValue)> {
        (0..KEYS_PER_SHAPE)
            .map(|index| entry(&format!("{tag}-{index}")))
            .collect()
    }

    /// Persistent worker threads: they wait on `start`, run one batch each,
    /// and report on `done`. Spawning happens once, outside the measured
    /// region, so a measurement is the writes and nothing else.
    struct Fleet {
        start: Arc<Barrier>,
        done: Arc<Barrier>,
        stop: Arc<AtomicBool>,
        workers: Vec<std::thread::JoinHandle<()>>,
    }

    impl Fleet {
        fn new<F>(threads: usize, body: F) -> Self
        where
            F: Fn(usize) + Send + Sync + 'static,
        {
            let start = Arc::new(Barrier::new(threads + 1));
            let done = Arc::new(Barrier::new(threads + 1));
            let stop = Arc::new(AtomicBool::new(false));
            let body = Arc::new(body);
            let workers = (0..threads)
                .map(|index| {
                    let start = Arc::clone(&start);
                    let done = Arc::clone(&done);
                    let stop = Arc::clone(&stop);
                    let body = Arc::clone(&body);
                    std::thread::spawn(move || {
                        loop {
                            start.wait();
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            body(index);
                            done.wait();
                        }
                    })
                })
                .collect();
            Self {
                start,
                done,
                stop,
                workers,
            }
        }

        /// Run one batch: every worker performs its writes, then all return.
        fn batch(&self) {
            self.start.wait();
            self.done.wait();
        }
    }

    impl Drop for Fleet {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.start.wait();
            for worker in self.workers.drain(..) {
                worker.join().expect("join a worker thread");
            }
        }
    }

    /// Shape A: every write takes the one shared mutex.
    fn bench_mutex(
        c: &mut Criterion,
        cache: DnsCacheMap,
        entries: Arc<Vec<(CacheKey, CacheValue)>>,
    ) {
        let shared = Arc::new(Mutex::new(cache));
        let mut base = 0;
        for threads in THREAD_COUNTS {
            let run_base = base;
            base += threads * INSERTS_PER_THREAD;
            let fleet_shared = Arc::clone(&shared);
            let fleet_entries = Arc::clone(&entries);
            let fleet = Fleet::new(threads, move |index| {
                let start = run_base + index * INSERTS_PER_THREAD;
                for entry in &fleet_entries[start..start + INSERTS_PER_THREAD] {
                    // `blocking_lock` is this shape's honest measurement: the
                    // server takes the same mutex per write, one task at a
                    // time, and parks when another worker holds it.
                    let mut cache = fleet_shared.blocking_lock();
                    cache.insert(&entry.0, &entry.1).expect("insert entry");
                }
            });
            // The first batch of every shape pays the first-write work that in
            // the server happens on start-up, not per query.
            fleet.batch();

            let mut group = c.benchmark_group(format!("cache_write/a_mutex/{threads}_threads"));
            group.throughput(Throughput::Elements((threads * INSERTS_PER_THREAD) as u64));
            group.bench_function("batch", |b| b.iter(|| fleet.batch()));
            group.finish();
        }
    }

    thread_local! {
        /// This thread's own map handle, taken on its first write — the shape
        /// the middleware uses.
        static HANDLE: RefCell<Option<DnsCacheMap>> = const { RefCell::new(None) };
    }

    /// Shape B: each thread duplicates the handle once and writes through the
    /// thread-local, with no lock.
    fn bench_per_thread(
        c: &mut Criterion,
        template: DnsCacheMap,
        entries: Arc<Vec<(CacheKey, CacheValue)>>,
    ) {
        let template = Arc::new(template);
        let mut base = 0;
        for threads in THREAD_COUNTS {
            let run_base = base;
            base += threads * INSERTS_PER_THREAD;
            let fleet_template = Arc::clone(&template);
            let fleet_entries = Arc::clone(&entries);
            let fleet = Fleet::new(threads, move |index| {
                let start = run_base + index * INSERTS_PER_THREAD;
                for entry in &fleet_entries[start..start + INSERTS_PER_THREAD] {
                    HANDLE.with(|slot| {
                        let mut slot = slot.borrow_mut();
                        if slot.is_none() {
                            *slot = Some(
                                fleet_template
                                    .try_clone()
                                    .expect("duplicate the cache handle"),
                            );
                        }
                        slot.as_mut()
                            .expect("initialised above")
                            .insert(&entry.0, &entry.1)
                            .expect("insert entry");
                    });
                }
            });
            fleet.batch();

            let mut group =
                c.benchmark_group(format!("cache_write/b_per_thread/{threads}_threads"));
            group.throughput(Throughput::Elements((threads * INSERTS_PER_THREAD) as u64));
            group.bench_function("batch", |b| b.iter(|| fleet.batch()));
            group.finish();
        }
    }

    pub fn run() -> Result<()> {
        let setup = setup()?;
        println!(
            "cache_write_bench: {} entries/thread, map capacity {MAP_ENTRIES}, pin {}",
            INSERTS_PER_THREAD,
            setup.pin.display()
        );

        let entries_a = Arc::new(entries("a"));
        let entries_b = Arc::new(entries("b"));
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(500))
            .measurement_time(Duration::from_secs(2))
            .configure_from_args();
        bench_mutex(&mut criterion, setup.mutex_shape, Arc::clone(&entries_a));
        bench_per_thread(&mut criterion, setup.header, Arc::clone(&entries_b));
        criterion.final_summary();
        // The pin path holds a map only this process was using.
        let _ = std::fs::remove_file(&setup.pin);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux_bench::run() {
        println!("cache_write_bench: nothing measured, setup failed: {error:#}");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("cache_write_bench: needs Linux and root; nothing measured");
}

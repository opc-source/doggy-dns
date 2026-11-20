# 运行时拓扑拆分 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 DNS 请求路径放进独立 OS 线程上的负载 runtime，让控制面、XDP 观测、pprof 与关停编排跑在主线程的 `current_thread` runtime 上，并用一个进程级冒烟测试钉住启动/关停与线程落点。

**Architecture:** `main()` 建 `new_current_thread()` 管理 runtime 并在主线程 `block_on(async_main)`；`async_main` 在 handler 就绪后 spawn `doggy-dns-pool` OS 线程，线程内建 `multi_thread(worker_threads)` 负载 runtime，整段完成 `build_server`（绑口）与 `block_until_done`。跨 runtime 只用 `started`(oneshot) / `done`(mpsc(1)) / `stop`(watch) 交接，管理侧不持有负载池的 `Handle`。

**Tech Stack:** Rust 2024、Tokio 1.54（`current_thread` + `multi_thread`）、hickory-server 0.26、std `thread::Builder`；冒烟测试用 `kill -TERM` 发信号，**不新增依赖**。

**Spec:** [docs/superpowers/specs/2026-11-20-runtime-topology-design.md](../specs/2026-11-20-runtime-topology-design.md)

> **Post-review change:** the load pool's OS thread was removed after this plan
> ran. `main` now builds and owns the DNS runtime and spawns the serve task by
> `Handle`; `src/main.rs` was split into `dns.rs`, `plugins.rs` and
> `control_plane.rs`, and the "load" naming became "DNS" (`dns::build_runtime`,
> `dns::start_server`, `dns::ServerHandle`). The rest of this document is the
> historical record.

## Global Constraints

- Rust edition 2024；`rustfmt` max_width 100；CI 用 `cargo clippy --all-features --all-targets -- -D warnings`。
- 非测试代码禁止 `.unwrap()`、`std::time::Instant`、`std::thread::sleep`（`.clippy.toml`）；测试文件在 crate 根写 `#![allow(clippy::disallowed_methods)]`。
- 集成测试放 `tests/`，不使用内联 `#[cfg(test)]`。
- 不改配置 schema、`validate_config`、`worker_threads` 语义、`shutdown_timeout` / 2 秒 background grace。
- 保留启动/关停日志措辞：`doggy-dns is ready`、`doggy-dns shut down gracefully`、`shutdown drain timed out ... forcing exit`。
- 线程命名固定：OS 线程 `doggy-dns-pool`，worker `doggy-dns-worker-{id}`（id 来自 `AtomicUsize`）。
- `Nacos` 后台任务与 OTLP exporters 不动：它们各有自己的 runtime/线程。
- 本任务不新增依赖，**不要动 `Cargo.toml` / `Cargo.lock`**：工作区里 `Cargo.lock` 已有本任务之外的未提交改动。
- 不修改 `docs/benchmarks/macos-local-test-report.md`（历史报告里的旧线程名是当时的观测记录）。

---

### Task 1: 负载池拆分 + 进程级冒烟测试

**Files:**
- Create: `tests/runtime_smoke_test.rs`
- Modify: `src/main.rs`（`main`、`async_main`、新增负载池 helpers、`configure_cpu_affinity`）

**Interfaces:**
- Consumes: `doggy_dns_core::server::build_server`、`DoggyDnsHandler`、`Observability`、`xdp::init/start_stats`（均已有，签名不变）。
- Produces（同文件内使用；后续任务只依赖文档文本，不依赖这些名字）:
  - `struct LoadPool { stop: tokio::sync::watch::Sender<bool>, done: tokio::sync::mpsc::Receiver<Result<()>>, thread: std::thread::JoinHandle<()> }`
  - `async fn spawn_load_pool(config: DoggyDnsConfig, handler: DoggyDnsHandler) -> Result<LoadPool>`
  - `fn run_load_pool(config: DoggyDnsConfig, handler: DoggyDnsHandler, started: tokio::sync::oneshot::Sender<Result<()>>, done: tokio::sync::mpsc::Sender<Result<()>>, stop: tokio::sync::watch::Receiver<bool>)`
  - `fn build_load_runtime(config: &DoggyDnsConfig) -> Result<tokio::runtime::Runtime>`
  - `async fn wait_for_stop(stop: &mut tokio::sync::watch::Receiver<bool>)`
  - `enum PoolExit { Server(Result<()>), Stop }`
  - `fn worker_index_from_thread_name() -> Option<usize>`
  - `async_main` 返回类型改为 `Result<Option<std::thread::JoinHandle<()>>>`

- [ ] **Step 1: 写冒烟测试（完整文件）**

创建 `tests/runtime_smoke_test.rs`：

```rust
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // tests may unwrap and sleep on a child process

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// Both listeners pick an ephemeral port; the control plane logs the address it
/// actually bound, and it only starts once every DNS socket is listening.
const CONFIG: &str = r#"
[server]
listen_addr = "127.0.0.1"
port = 0
worker_threads = 1
shutdown_timeout = 2

[control]
enabled = true
listen_addr = "127.0.0.1"
port = 0
endpoints = ["healthz"]
"#;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn load_pool_and_management_runtime_start_and_stop() {
    let config_path = std::env::temp_dir().join(format!(
        "doggy-dns-runtime-smoke-{}.toml",
        std::process::id()
    ));
    std::fs::write(&config_path, CONFIG).expect("write smoke-test config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_doggy-dns"))
        .arg(&config_path)
        .env_remove("RUST_LOG")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn doggy-dns");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut guard = KillOnDrop(child);

    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    for stream in [
        Box::new(stdout) as Box<dyn Read + Send>,
        Box::new(stderr) as Box<dyn Read + Send>,
    ] {
        let lines_tx = lines_tx.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while let Ok(read) = reader.read_line(&mut line) {
                if read == 0 {
                    break;
                }
                let _ = lines_tx.send(std::mem::take(&mut line));
            }
        });
    }
    drop(lines_tx);

    let mut seen = String::new();
    let mut control_port = None;
    for _ in 0..300 {
        match lines_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if let Some(port) = control_port_from_log(&line) {
                    control_port = Some(port);
                    seen.push_str(&line);
                    break;
                }
                seen.push_str(&line);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let control_port =
        control_port.unwrap_or_else(|| panic!("control plane never listened; output:\n{seen}"));

    let response = get_healthz(control_port);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected /healthz response:\n{response}"
    );

    #[cfg(target_os = "linux")]
    {
        let names = thread_names(guard.0.id()).expect("read /proc thread names");
        assert!(
            names.iter().any(|name| name == "doggy-dns-pool"),
            "no load pool thread; threads: {names:?}"
        );
        assert!(
            names.iter().any(|name| name.starts_with("doggy-dns-worke")),
            "no load worker thread; threads: {names:?}"
        );
        assert!(
            !names
                .iter()
                .any(|name| name.starts_with("doggy-dns-runti")),
            "the old single-runtime name is still present; threads: {names:?}"
        );
    }

    let kill = Command::new("kill")
        .arg("-TERM")
        .arg(guard.0.id().to_string())
        .status()
        .expect("run kill");
    assert!(kill.success(), "failed to send SIGTERM: {kill}");

    let mut status = None;
    for _ in 0..200 {
        if let Some(exit) = guard.0.try_wait().expect("try_wait") {
            status = Some(exit);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut output = String::new();
    while let Ok(line) = lines_rx.recv_timeout(Duration::from_millis(500)) {
        output.push_str(&line);
    }

    let status = status
        .unwrap_or_else(|| panic!("doggy-dns did not exit after SIGTERM; output:\n{seen}{output}"));
    assert!(
        status.success(),
        "doggy-dns exited with {status}:\n{output}"
    );
    assert!(
        output.contains("doggy-dns shut down gracefully"),
        "expected a graceful drain after SIGTERM; output:\n{output}"
    );
    assert!(
        !output.contains("forcing exit"),
        "the drain was forced instead of graceful; output:\n{output}"
    );

    std::fs::remove_file(&config_path).expect("remove smoke-test config");
}

fn control_port_from_log(line: &str) -> Option<u16> {
    if !line.contains("control plane listening") {
        return None;
    }
    let addr = line.split_once("addr=")?.1.split_whitespace().next()?;
    addr.rsplit_once(':')?.1.parse().ok()
}

fn get_healthz(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to /healthz");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("send /healthz request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read /healthz response");
    response
}

#[cfg(target_os = "linux")]
fn thread_names(pid: u32) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        let comm = std::fs::read_to_string(entry?.path().join("comm"))?;
        names.push(comm.trim_end().to_string());
    }
    Ok(names)
}
```

- [ ] **Step 2: 跑测试确认红（Linux）/验证测试夹具（macOS）**

Run: `cargo test --test runtime_smoke_test -- --nocapture`

Expected:
- Linux：FAIL，`no load pool thread; threads: ["doggy-dns", "doggy-dns-runti"]`（旧拓扑只有单个 runtime 线程）。这就是拓扑的红检。
- macOS：`#[cfg(target_os = "linux")]` 段跳过，测试在旧代码上 PASS；macOS 的红检由 Step 10 的变异检查承担。

- [ ] **Step 3: 调整 imports**

在 `src/main.rs` 的 `use std::sync::Arc;` 之前插入：

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
```

（`std::sync::Arc` 原样保留；其余 import 不变。）

- [ ] **Step 4: 重写 `main()`**

用下面内容替换现有 `fn main() -> Result<()> { ... }` 整个函数（原文件第 59–121 行附近）：

```rust
fn main() -> Result<()> {
    // Load config before initializing tracing so log format and OTLP settings
    // are available to the subscriber.
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/doggy-dns.toml".to_string());
    let (config, config_content) =
        load_config(&config_path).context(format!("failed to load config from {}", config_path))?;

    let mut telemetry = Observability::init(&config, config.middleware.log_format.as_deref())
        .context("failed to initialize observability")?;

    tracing::info!("doggy-dns starting...");

    tracing::info!(
        listen_addr = %config.server.listen_addr,
        port = config.server.port,
        worker_threads = config.server.worker_threads,
        reuseport_listen_num = config.server.reuseport_listen_num,
        cpu_affinity = ?config.server.cpu_affinity,
        tcp_timeout = config.server.tcp_timeout,
        shutdown_timeout = config.server.shutdown_timeout,
        tls_enabled = config.server.tls.as_ref().is_some_and(|t| t.enabled),
        https_enabled = config.server.https.as_ref().is_some_and(|h| h.enabled),
        logging = config.middleware.logging,
        plugin_count = config.plugins.len(),
        remote_config_enabled = config.remote_config.enabled,
        observability_enabled = config.observability.enabled,
        "loaded configuration"
    );

    for (i, plugin) in config.plugins.iter().enumerate() {
        tracing::info!(
            index = i,
            kind = ?plugin.kind,
            enabled = plugin.enabled,
            "plugin configured"
        );
    }

    // Chores run on the main thread: control plane, XDP polling, pprof and
    // shutdown orchestration. DNS serving gets its own runtime on its own OS
    // thread (see `spawn_load_pool`).
    let management = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build the management runtime")?;
    tracing::info!("management runtime built");

    let load_thread = management.block_on(async_main(config, &config_content, &mut telemetry))?;

    // A clean drain leaves the load pool thread finishing its own runtime
    // shutdown; after a forced exit the process reclaims it.
    if let Some(thread) = load_thread {
        let _ = thread.join();
    }

    telemetry.shutdown();
    Ok(())
}
```

- [ ] **Step 5: 重写 `async_main()`**

用下面内容替换现有 `async fn async_main(...)` 整个函数（原文件第 123–230 行附近）：

```rust
async fn async_main(
    config: DoggyDnsConfig,
    config_content: &str,
    telemetry: &mut Observability,
) -> Result<Option<std::thread::JoinHandle<()>>> {
    // Profiles need a Tokio runtime for their HTTP exporter.
    telemetry.spawn_profiles();

    // Build middleware chain
    let mut middlewares: Vec<Arc<dyn Middleware>> = Vec::new();
    if config.middleware.logging {
        middlewares.push(Arc::new(LoggingMiddleware));
    }
    if let Some(metrics) = telemetry.metrics_middleware() {
        middlewares.push(metrics);
    }

    // Initialize the XDP fast path (Linux + `xdp` feature; a stub elsewhere).
    // The handle owns the manager, which must stay alive for the server
    // lifetime — dropping it detaches the XDP program from the interface.
    let mut xdp = xdp::init(&config, &mut middlewares);

    let xdp_task = xdp::start_stats(
        &mut xdp,
        telemetry.xdp_metrics(),
        Duration::from_secs(config.observability.metrics.interval_secs.max(1)),
    );

    // Build authority chain from plugins (watchers kept alive for subscriptions
    // and for the control plane's live cache signals).
    let (watchers, authority_chain) = build_authority_chain(&config).await?;

    // Create handler
    let handler = DoggyDnsHandler::new(
        middlewares,
        Arc::new(authority_chain),
        xdp.enabled(),
        AnswerRotation::new(config.server.rotate_answers),
    );

    // The kernel answers a cache hit without asking userspace again, so the
    // record order frozen into the cached reply is the one every hit replays
    // until the entry expires. Say it once, at startup, rather than letting it
    // look like rotation is broken.
    if xdp.enabled() && config.server.rotate_answers {
        tracing::warn!(
            "answer rotation applies to userspace answers only: an XDP cache hit replays the \
             order captured when the entry was written until its TTL expires"
        );
    }

    // Bind and serve on the dedicated load pool. This returns only once every
    // DNS socket is listening, so a bind failure is still a startup error.
    let LoadPool {
        stop: pool_stop,
        mut done,
        thread,
    } = spawn_load_pool(config.clone(), handler).await?;

    tracing::info!("doggy-dns is ready");

    // Start the control plane if enabled, keeping its port if the previous
    // process has not released it yet (non-fatal either way). The XDP
    // attach flag is fixed after startup — the manager lives as long as this
    // function and dropping it detaches the program.
    let live = build_live_state(&watchers, xdp.enabled());
    let mcp_handle = start_control_plane(&config, config_content, live).await;

    // Run server until shutdown signal
    let shutdown_timeout = Duration::from_secs(config.server.shutdown_timeout);

    // Wait for SIGINT or SIGTERM, then cancel the server
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = signal::ctrl_c() => tracing::info!("received SIGINT, shutting down..."),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down..."),
        result = done.recv() => {
            // Server stopped on its own (socket error, etc.)
            match result {
                Some(server_result) => server_result?,
                None => anyhow::bail!("dns load pool exited without reporting its result"),
            }
            tracing::info!("server stopped");
            return Ok(Some(thread));
        }
    }

    // Signal received — ask the load pool to drain, bounded by the timeout
    let _ = pool_stop.send(true);
    let drained = match tokio::time::timeout(shutdown_timeout, done.recv()).await {
        Ok(Some(server_result)) => {
            server_result?;
            tracing::info!("doggy-dns shut down gracefully");
            true
        }
        Ok(None) => anyhow::bail!("dns load pool exited without reporting its result"),
        Err(_) => {
            tracing::warn!(
                "shutdown drain timed out after {}s, forcing exit",
                config.server.shutdown_timeout
            );
            false
        }
    };

    // Stop the stats poller and let it perform its final map read before the
    // telemetry providers are flushed in main().
    if let Some((stop, handle)) = xdp_task {
        let _ = stop.send(true);
        await_shutdown("xdp stats poller", handle).await;
    }

    // Shut down the control plane listener
    if let Some(handle) = mcp_handle {
        handle.cancel.cancel();
        await_shutdown("mcp server", handle.wait()).await;
    }

    if drained {
        Ok(Some(thread))
    } else {
        Ok(None)
    }
}
```

- [ ] **Step 6: 新增负载池 helpers**

紧跟在 `async_main` 之后、`configure_cpu_affinity` 之前插入：

```rust
/// Management-side handles to the dedicated DNS load pool.
struct LoadPool {
    stop: tokio::sync::watch::Sender<bool>,
    done: tokio::sync::mpsc::Receiver<Result<()>>,
    thread: std::thread::JoinHandle<()>,
}

/// Start the load pool on its own OS thread and wait until every DNS socket is
/// bound.
///
/// Binding and serving must both happen inside the load runtime: hickory hands
/// each registered socket's accept/request tasks to whatever runtime is current
/// at registration (`Server::register_socket`), so a server built elsewhere and
/// driven here would keep its work on the wrong runtime.
async fn spawn_load_pool(config: DoggyDnsConfig, handler: DoggyDnsHandler) -> Result<LoadPool> {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let (done_tx, done_rx) = tokio::sync::mpsc::channel::<Result<()>>(1);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    let thread = std::thread::Builder::new()
        .name("doggy-dns-pool".to_string())
        .spawn(move || run_load_pool(config, handler, started_tx, done_tx, stop_rx))
        .context("failed to spawn the dns load pool thread")?;

    match started_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.context("failed to start the dns server")),
        Err(_) => anyhow::bail!("dns load pool exited before the server was built"),
    }

    Ok(LoadPool {
        stop: stop_tx,
        done: done_rx,
        thread,
    })
}

fn run_load_pool(
    config: DoggyDnsConfig,
    handler: DoggyDnsHandler,
    started: tokio::sync::oneshot::Sender<Result<()>>,
    done: tokio::sync::mpsc::Sender<Result<()>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let runtime = match build_load_runtime(&config) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };

    runtime.block_on(async move {
        let mut server = match build_server(&config, handler).await {
            Ok(server) => server,
            Err(error) => {
                let _ = started.send(Err(error));
                return;
            }
        };

        if started.send(Ok(())).is_err() {
            // Management is gone; there is nobody to serve or report to.
            return;
        }

        let exit = tokio::select! {
            result = server.block_until_done() => {
                PoolExit::Server(result.map_err(anyhow::Error::from))
            }
            _ = wait_for_stop(&mut stop) => PoolExit::Stop,
        };

        let result = match exit {
            PoolExit::Server(result) => result,
            PoolExit::Stop => server
                .shutdown_gracefully()
                .await
                .map_err(anyhow::Error::from),
        };
        let _ = done.send(result).await;
    });
}

enum PoolExit {
    Server(Result<()>),
    Stop,
}

/// Resolve when management asks the pool to stop. A dropped sender means
/// management is gone, which is also a reason to stop serving.
async fn wait_for_stop(stop: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = stop.wait_for(|requested| *requested).await;
}

fn build_load_runtime(config: &DoggyDnsConfig) -> Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .thread_name_fn(|| {
            static NEXT_WORKER: AtomicUsize = AtomicUsize::new(0);
            format!(
                "doggy-dns-worker-{}",
                NEXT_WORKER.fetch_add(1, Ordering::Relaxed)
            )
        })
        .worker_threads(config.server.worker_threads);

    if let Some(affinity) = &config.server.cpu_affinity {
        configure_cpu_affinity(&mut builder, affinity)?;
    }

    let runtime = builder
        .build()
        .context("failed to build the dns load runtime")?;
    tracing::info!(
        worker_threads = config.server.worker_threads,
        "dns load runtime built"
    );
    Ok(runtime)
}
```

- [ ] **Step 7: 替换 `configure_cpu_affinity` 并新增名字解析**

用下面内容替换现有 `fn configure_cpu_affinity(...)` 整个函数（原文件第 232–267 行附近）：

```rust
/// Configure CPU affinity for the load pool's worker threads.
///
/// Each worker pins itself to the core at its own index: the index is parsed
/// from the thread name (`doggy-dns-worker-{id}`) inside `on_thread_start`, so
/// a thread's label and its pinned core always agree.
fn configure_cpu_affinity(
    builder: &mut tokio::runtime::Builder,
    affinity: &Vec<usize>,
) -> Result<()> {
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    for &core_idx in affinity {
        anyhow::ensure!(
            core_idx < core_ids.len(),
            "cpu_affinity contains core index {core_idx}, but only {} cores available",
            core_ids.len()
        );
    }

    tracing::info!(cpu_affinity = ?affinity, "CPU affinity configured");
    let affinity = affinity.clone();
    builder.on_thread_start(move || {
        let Some(worker_index) = worker_index_from_thread_name() else {
            return;
        };
        let Some(core_id) = affinity
            .get(worker_index)
            .and_then(|&core_idx| core_ids.get(core_idx))
            .copied()
        else {
            return;
        };
        if core_affinity::set_for_current(core_id) {
            tracing::info!(
                worker_index,
                core_id = core_id.id,
                "pinned worker thread to CPU core"
            );
        }
    });

    Ok(())
}

/// The `{id}` in `doggy-dns-worker-{id}`.
fn worker_index_from_thread_name() -> Option<usize> {
    std::thread::current()
        .name()
        .and_then(|name| name.strip_prefix("doggy-dns-worker-"))
        .and_then(|index| index.parse().ok())
}
```

- [ ] **Step 8: 跑冒烟测试确认绿**

Run: `cargo test --test runtime_smoke_test -- --nocapture`

Expected: PASS（Linux 上同时证明 `doggy-dns-pool` / `doggy-dns-worker-*` 线程存在且旧名消失）。

- [ ] **Step 9: 跑全量 gates**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
cargo bench --no-run --workspace
```

Expected: 全部通过。`doggy-dns-nacos` / `doggy-dns-plugin` 的 forward/native 用例会发真实 DNS 请求，需要网络；本机先前通过则保持通过。

- [ ] **Step 10: 变异检查（证明冒烟测试能抓到关停回归）**

临时把 `async_main` 里的这一行注释掉：

```rust
let _ = pool_stop.send(true);
```

Run: `cargo test --test runtime_smoke_test -- --nocapture`

Expected: FAIL，断言信息为 `expected a graceful drain after SIGTERM`（日志里出现 `forcing exit`）。
恢复该行，再跑一次，Expected: PASS。

- [ ] **Step 11: 提交**

只提交本任务的两个文件，**不要 `git add Cargo.toml Cargo.lock`**（工作区里 lock 已有本任务之外的改动）：

```bash
git add src/main.rs tests/runtime_smoke_test.rs
git commit -m "$(cat <<'EOF'
refactor: serve dns from a dedicated load pool thread

The DNS worker pool now runs on its own OS thread and runtime, and the
main thread carries a current_thread management runtime for the control
plane, XDP polling, pprof and shutdown orchestration. Binding and
serving stay together because hickory spawns each socket's tasks onto
the runtime current at registration time.

Workers are named doggy-dns-worker-{id}; cpu_affinity pins by that id,
so a thread's label and its core can never disagree. A process-level
smoke test drives the real binary: /healthz on the management runtime,
thread placement on Linux, SIGTERM and a graceful drain.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 2: 文档

**Files:**
- Modify: `docs/architecture.md:114`
- Modify: `docs/configuration.md`（CPU affinity 段）
- Modify: `AGENTS.md`（Server 段末句）

**Interfaces:**
- Consumes: Task 1 实际实现的行为（命名、绑核来源、`mpsc` 完成通道）。
- Produces: 与实现一致的文档；无代码接口。

- [ ] **Step 1: `docs/architecture.md`**

把：

```markdown
The tokio runtime is built manually in `main()` with configurable `worker_threads`.
```

替换为：

```markdown
Two Tokio runtimes are built manually in `main()`. A single-threaded management
runtime on the main thread runs the control plane, XDP polling, pprof and shutdown
orchestration. The DNS load pool runs on its own OS thread (`doggy-dns-pool`) with
`worker_threads` workers named `doggy-dns-worker-{id}`; it binds every DNS socket
and serves it. `cpu_affinity` applies to those load workers only.
```

- [ ] **Step 2: `docs/configuration.md` CPU affinity 段**

把：

```markdown
`cpu_affinity` gives each tokio worker thread its own core, so a worker keeps that
core's caches for the process lifetime instead of being re-placed wherever the
scheduler finds room. The list must be as long as `worker_threads` with unique core
ids; threads claim entries in start order, so the core *set* is fixed while which
worker lands on which core is not.
```

替换为：

```markdown
`cpu_affinity` gives each DNS worker thread its own core, so a worker keeps that
core's caches for the process lifetime instead of being re-placed wherever the
scheduler finds room. The list must be as long as `worker_threads` with unique core
ids; worker `i` (thread name `doggy-dns-worker-i`) is pinned to `cpu_affinity[i]`.
```

再把段末的：

```markdown
Blocking threads,
such as CPU profile collection, draw from the same list round-robin and can share a
worker's core.
```

替换为：

```markdown
The management runtime
(control plane, XDP polling, CPU profile collection) is not pinned and can share any
core, including a DNS worker's.
```

- [ ] **Step 3: `AGENTS.md`**

在 `### Server (`core/server.rs`)` 小节末尾，把同一句：

```markdown
The tokio runtime is built manually in `main()` with configurable `worker_threads`.
```

替换为 Step 1 的同一段文字（两处保持一致）。

- [ ] **Step 4: 格式化检查**

Run: `cargo fmt --all -- --check`
Expected: PASS（文档改动不影响，但确认工作区仍干净）。

- [ ] **Step 5: 提交**

```bash
git add AGENTS.md docs/architecture.md docs/configuration.md
git commit -m "$(cat <<'EOF'
docs: describe the split load pool and management runtime

The affinity section now says pinning covers the DNS workers only, and
the worker-to-core mapping is stated as worker i -> cpu_affinity[i].

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 3: 最终验证与 PR 证据

**Files:** 无改动（发现问题则回到对应任务修复并补提交）。

**Interfaces:**
- Consumes: Task 1/2 的产物。
- Produces: PR 描述所需的实测输出。

- [ ] **Step 1: 全量 gates**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
cargo bench --no-run --workspace
```

Expected: 全部通过。

- [ ] **Step 2: 本机手工验证（后台起进程）**

用一个临时配置起服务（`worker_threads = 1`、固定端口），然后：

```bash
dig @127.0.0.1 -p <port> <service>.<group>.<ns>.<zone> A
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:<control_port>/healthz
curl -s http://127.0.0.1:<control_port>/readyz
ps -M <pid>            # macOS：确认 doggy-dns-pool / doggy-dns-worker-*
top -H -p <pid> -n 1   # Linux：同上
```

Expected: 查询有应答；`/healthz`、`/readyz` 均 200；线程列表中负载池与 worker 名符合 Task 1 的命名。

- [ ] **Step 3: SIGTERM 演练**

```bash
kill -TERM <pid>
```

Expected: 日志出现 `doggy-dns shut down gracefully`，进程退出码 0；配置了 `cpu_affinity` 时，日志里出现对应的 `pinned worker thread to CPU core`。

- [ ] **Step 4: 记录证据**

把 Step 1–3 的命令与输出摘要留作 PR 描述素材（不要提交日志文件）。

---

## Self-Review

- **Spec coverage:** 目标（Task 1）、拓扑（Task 1 Step 5–7）、交接（Task 1 Step 5–6；`done` 用 `mpsc` 已在 spec 中同步）、启动顺序（Task 1 Step 4–6）、任务归属（管理任务随 `main` 改为 `current_thread` 自然落在主线程，Task 1 Step 4）、关停（Task 1 Step 5 + Step 10 变异检查）、绑核/pprof（Task 1 Step 7；pprof 仍由 `telemetry.spawn_profiles()` 在管理侧启动）、命名（Task 1 Step 6–7 + Task 2 Step 1–2）、冒烟测试（Task 1 Step 1）、文档（Task 2）、验证（Task 3）。
- **Placeholder scan:** 无 TBD/TODO；每一步都有可执行命令或完整代码。
- **Type consistency:** `LoadPool`/`spawn_load_pool`/`run_load_pool`/`PoolExit`/`wait_for_stop`/`build_load_runtime`/`worker_index_from_thread_name` 在 Task 1 内一次定义、前后一致；`async_main` 返回 `Result<Option<JoinHandle<()>>>` 与 `main` 的解构一致；`done` 用 `mpsc::Receiver<Result<()>>`，`recv()` 在 `select!` 取消后可复用。
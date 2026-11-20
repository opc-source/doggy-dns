# 运行时拓扑拆分：DNS runtime 与管理线程

日期：2026-11-20
状态：待评审

## 1. 目标

- DNS runtime 的 worker 只跑 DNS 请求路径（hickory 的 UDP/TCP/DoT/DoH）。
- 控制面、XDP 观测、pprof、信号与关停编排收敛到管理侧。
- 不新增配置项；`worker_threads`、`cpu_affinity`、`shutdown_timeout` 的语义与取值不变。

## 2. 现状与证据

`main()` 建一个 `new_multi_thread().worker_threads(N)`（`src/main.rs:100-110`），
`async_main` 在该 runtime 上 `block_on`。同一个池里同时跑：

| 任务 | 位置 |
|------|------|
| hickory UDP/TCP/DoT/DoH | `crates/doggy-dns-core/src/server.rs` |
| 控制面 axum / MCP | `crates/doggy-dns-control/src/transport.rs:59,101` |
| XDP stats poller | `crates/doggy-dns-xdp/src/stats/reader.rs:65` |
| XDP kernel-log drain | `src/xdp/linux.rs:44` |
| pprof task + `spawn_blocking` 采集 | `crates/doggy-dns-observability/src/profiles/mod.rs:110,123` |
| 信号、关停编排 | `src/main.rs:186-229` |

影响设计的事实（均已核对源码）：

- hickory 在 `register_socket`/`register_listener` 时就把 accept/请求任务 spawn 到
  **当时**的 runtime（`hickory-server-0.26.3/src/server/mod.rs:102-105,126`），
  `block_until_done` 只是等待（同文件 `:418`）。因此绑端口与建 Server 必须整段在
  DNS runtime 上执行，只挪 `block_until_done` 无效。
- tokio 的 `on_thread_start` 对 blocking 线程同样回调
  （`tokio-1.54.1/src/runtime/blocking/pool.rs:758`），今天 pprof 的采集线程会占用
  一个 `cpu_affinity` 索引并与 worker 抢核。
- Nacos SDK 自带 `nacos-client-thread-pool` runtime（锁定的 git rev `531848f`，
  `src/common/executor/mod.rs:16-24`，默认 1 个 worker），其后台维护本来就不在 DNS runtime。
- pprof 0.15 用进程级 `ITIMER_PROF`（`pprof-0.15.0/src/timer.rs:43`），SIGPROF 落到
  当时在跑 CPU 的线程，样本带线程名/线程 id（`profiler.rs:471`），采集线程放在管理侧
  不会丢失 DNS worker 的样本。

## 3. 设计

### 3.1 拓扑

```text
main thread ── current_thread runtime（管理）── block_on(async_main)
                 ├─ control plane (axum/rmcp, /healthz /readyz /mcp)
                 ├─ XDP stats poller / kernel-log drain
                 ├─ pprof task（spawn_blocking 采集，进程级采样）
                 ├─ 信号 + 关停编排
                 └─ Nacos 构建期 await（SDK 自身 runtime 跑后台）

main 持有的 multi_thread(worker_threads) runtime（DNS）
                 ├─ thread_name_fn → doggy-dns-worker-{id}
                 ├─ on_thread_start → cpu_affinity[id]（只作用于这里）
                 └─ Handle::spawn → build_server + block_until_done
```

管理 runtime 用 `new_current_thread().enable_all()` 跑在主线程，不新增常驻线程。
两个 runtime 都由 `main()` 构建；DNS runtime 的 `Handle` 交给 `async_main` 投递
serve 任务，runtime 本体由 `main` 持有并在管理 runtime 结束后 shutdown。

### 3.2 跨 runtime 交接

- `started`: `oneshot<Result<(), anyhow::Error>>`，bind 成功后才发。
- `done`: `mpsc::channel(1)` 的发送/接收端，`block_until_done` 或
  `shutdown_gracefully` 结束时发一次；线程 panic 导致 sender 关闭时管理侧按错误处理。
  用 mpsc 而非 oneshot：tokio 1.54 的 oneshot 只有按值的 `Future`，被 `select!`
  取消后不能再次 await，而 `mpsc::Receiver::recv` 是 cancel-safe 的。
- `stop`: `watch::Sender<bool>` / `Receiver<bool>`；管理侧置 `true`，DNS server 任务用
  `wait_for(|v| *v)` 等待后调用 `server.shutdown_gracefully()`。

### 3.3 启动顺序

1. `main` 读配置、`Observability::init`（原样）。
2. `main` 建 `multi_thread(worker_threads)` DNS runtime（`thread_name_fn` 用
   `AtomicUsize` 生成 `doggy-dns-worker-{id}`；`on_thread_start` 从线程名解析 id，
   绑定 `cpu_affinity[id]`，命名与绑核同源），再建 current_thread 管理 runtime，
   把DNS runtime 的 `Handle` 传进 `async_main`。
3. `async_main`：`spawn_profiles()` → middleware → `xdp::init` / `start_stats` →
   `build_authority_chain`（Nacos SDK build 的 await）→ `DoggyDnsHandler`。
4. `Handle::spawn` 投递 serve 任务，移入 `config.clone()`、handler、`started`/`done`
   sender、`stop` receiver。
5. serve 任务里 `build_server`（UDP × `reuseport_listen_num`、TCP、DoT、DoH）→
   `started Ok` → `select!` 等待 server 结束或 stop → 需要时
   `shutdown_gracefully()` → `done`。
6. 管理侧收到 `started Ok` 后起控制面并打 `doggy-dns is ready`；失败则返回 Err，
   进程非零退出（与今天 bind 失败一致）。

`select!` 只产出退出原因，`shutdown_gracefully()` 在 select 之外调用，避免
`&mut server` 的双重借用：

```rust
enum Exit { Server(Result<(), anyhow::Error>), Stop }
let exit = tokio::select! {
    result = server.block_until_done() => Exit::Server(result.map_err(Into::into)),
    _ = wait_for_stop(&mut stop_rx) => Exit::Stop,
};
let result = match exit {
    Exit::Server(result) => result,
    Exit::Stop => server.shutdown_gracefully().await.map_err(Into::into),
};
let _ = done_tx.send(result).await;
```

### 3.4 任务归属

| 任务 | 原位置 | 之后 |
|------|--------|------|
| hickory UDP/TCP/DoT/DoH | 共享 runtime | DNS runtime |
| 控制面 axum / MCP | 共享 runtime | 管理 |
| XDP stats poller、kernel-log drain | 共享 runtime | 管理 |
| pprof task + 采集线程 | 共享 runtime | 管理 |
| 信号、关停编排 | 共享 runtime | 管理（主线程） |
| Nacos SDK 后台 | 自身 runtime | 不变 |
| OTLP exporters | 自身线程 | 不变 |

### 3.5 关停

- 信号分支：`stop = true` → `timeout(shutdown_timeout, done)`；成功打 graceful 日志，
  超时 warn `forcing exit` 后返回。
- server 自行结束或出错（`done` 先完成）：按原逻辑直接处理结果。
- 之后按原顺序停止 XDP poller（含最后一次 map 读）与控制面，各 2 秒 grace。
- 管理 runtime 返回后，`main` 对DNS runtime 调 `shutdown_timeout`（drain 正常）
  或 `shutdown_background`（超时，不等待），再 `telemetry.shutdown()`。

### 3.6 绑核与 pprof

- `cpu_affinity` 只挂在DNS runtime 的 builder 上；管理线程（主线程）不绑核，
  允许落在任意核，包括 DNS 绑核集合中的核——这是显式取舍。
- pprof `ProfilerGuard` 在管理侧创建；采样仍是进程级，DNS worker 覆盖不变。
- DNS runtime 没有 blocking 任务，今天 pprof 采集线程挤占绑核索引的问题消失。

### 3.7 命名

- worker：`doggy-dns-worker-{id}`；没有独立的 pool OS 线程。
- 管理侧运行在主线程，保持进程名。
- 原 worker 名 `doggy-dns-runtime` 不再使用；按旧名过滤的运维脚本需同步。

## 4. 冒烟测试

新增 `tests/runtime_smoke_test.rs`（`#[cfg(unix)]`），直接驱动真实二进制：

- 临时配置：`server.port = 0`、`worker_threads = 1`、`shutdown_timeout = 2`；
  `[control] enabled = true, listen_addr = "127.0.0.1", port = 0,
  endpoints = ["healthz"]`；无 `[[plugins]]`（默认 forward，不发起网络）。
- 用 `env!("CARGO_BIN_EXE_doggy-dns")` 启进程，读日志直到 `control plane listening`，
  从 `addr=127.0.0.1:<port>` 解析真实端口（`transport.rs:123` 打印）。
- 用 `std::net::TcpStream` 发最小 `GET /healthz`，断言 200。控制面只在 `started Ok`
  之后启动，因此 200 同时证明 DNS 端口绑定成功。
- 用 `kill -TERM <pid>` 发信号（无新增依赖），在
  `shutdown_timeout + 2s grace + 余量` 内断言退出码 0。
- 测试持有子进程的清理守卫，断言失败时不泄漏进程。
- 红检：去掉 stop 传播（DNS server 任务不取消）后测试必须超时失败。

## 5. 文档

- `docs/architecture.md:114`：改为 DNS runtime `worker_threads` + 管理 current_thread 的描述。
- `docs/configuration.md` CPU affinity 段：说明只作用于 DNS worker，管理线程不绑核。
- `AGENTS.md` 末段"tokio runtime is built manually in main()"同句更新。
- 若 README / `docs/development.md` 有线程或测试清单描述，同步。

## 6. 非目标与已知限制

- 不迁移 Nacos 后台任务：SDK 自带独立 runtime。
- 不处理 hot reload 的任务归属；工作区正在改 `config_watcher.rs`，未来 reload 触发器
  应显式放管理侧。
- 管理侧 `current_thread`：一次慢 MCP 调用会推迟 XDP 轮询、pprof 定时器与其它杂务，
  不影响 DNS；确有需要时再升级为 `multi_thread(1)`。
- DNS runtime 若将来引入 `spawn_blocking`，其 blocking 线程会继承
  `doggy-dns-worker-` 前缀，但 id ≥ `worker_threads`，`cpu_affinity` 索引覆盖不到它，
  因此不会被绑核。

## 7. 验证

- gates：`cargo fmt --all -- --check`、`cargo clippy --all-features --all-targets -- -D warnings`、
  `cargo test --workspace`、`cargo bench --no-run --workspace`。
- 冒烟测试；本机实跑 dig、`/healthz`、`/readyz`、SIGTERM，`top -H` 核对线程名与绑核。
- XDP 仅在 Linux CI 编译验证（cfg 未改动）。

## 8. 变更清单

- `src/main.rs`：入口、管理 runtime、`async_main` 与关停编排。
- `src/dns.rs`：DNS runtime 构建、worker 命名、绑核与 serve 任务交接。
- `src/plugins.rs`：权威链构建与默认 forward 回退。
- `src/control_plane.rs`：控制面 live 信号与监听器启动。
- `tests/runtime_smoke_test.rs`（新增，无新增依赖）。
- 文档三处（见第 5 节）。

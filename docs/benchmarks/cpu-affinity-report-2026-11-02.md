# CPU Affinity (线程绑核) A/B 性能测试报告

**测试日期**: 2026-11-02  
**测试工具**: dnsperf v2.14.0  
**数据来源**: 全部实测，每组配置运行 30 秒 × 3 次取中位数  

## Machine specification

| Item | Value |
|---|---|
| Vendor / product | Alibaba Cloud (`Alibaba Cloud ECS`), KVM guest (`systemd-detect-virt`: `kvm`) |
| CPU | Intel(R) Xeon(R) Platinum, 8 vCPU = 1 socket × 4 cores × 2 threads |
| CPU topology | Core 0: vCPU 0,1 · Core 1: vCPU 2,3 · Core 2: vCPU 4,5 · Core 3: vCPU 6,7 |
| Relevant CPU features | `avx512f avx512dq avx512bw avx512vl avx2 avx bmi1 bmi2 aes fma sse4_2` |
| Caches | L1d 32 KiB × 4, L1i 32 KiB × 4, L2 1 MiB × 4, L3 33 MiB (shared) |
| Memory | `MemTotal` 15,555,520 kB (14.83 GiB); **no swap** |
| Root disk | `/dev/vda3` 40 GiB virtio |
| OS | Alibaba Cloud Linux 4.0.6 (`ID=alinux`, OpenAnolis Edition) |
| Kernel | `Linux 6.6.102-8.alnx4.x86_64 #1 SMP PREEMPT_DYNAMIC x86_64` |
| dnsperf | 2.14.0 |

## 测试目的

验证 CPU 线程绑核（`cpu_affinity`）对 doggy-dns 性能的影响，
特别关注 **dnsperf 压测进程与 doggy-dns 服务进程的 CPU 竞争** 对测试结果的干扰。

## 背景：初次测试的误导

初次 A/B 测试（无隔离）显示绑核**降低**性能 2.6%~8.2%。
但深入分析发现，该结果存在**混淆变量**：

- 绑核后 doggy-dns 被限制在指定核心
- dnsperf 仍自由调度，OS 倾向于将其分配到相同核心
- 两个进程**争抢同一组 CPU 核心**，导致性能下降

因此，初次测试实际测量的是 **CPU 竞争** 的影响，而非绑核本身的效果。

## 本次测试：工作负载隔离

### 核心思路

通过 `taskset` 将 dnsperf 也绑定到**不同的物理核心**，消除竞争变量：

| 场景 | doggy-dns 绑核 | dnsperf 绑核 | 说明 |
|----|:-:|:-:|---|
| Baseline | 无 | 无 | 两个进程自由调度 |
| **Isolated** | 指定核心 | **不同物理核心** | 完全隔离，消除竞争 |
| Contention | 指定核心 | **相同核心** | 确认竞争影响 |

### 测试配置

| 参数 | 值 |
|---|-----|
| 监听地址 | 127.0.0.1:15353 |
| 插件 | forward → 8.8.8.8:53, 1.1.1.1:53 |
| XDP | 关闭 |
| 查询文件 | 20 个唯一域名 |
| 测试时长 | 30 秒 × 3 次 |
| 缓存预热 | 每轮 5 秒 warmup |
| **日志级别** | **`RUST_LOG=warn`**（减少日志 I/O 开销，避免影响性能数据） |

> **注意**: 本次测试使用 `RUST_LOG=warn` 启动 doggy-dns，相比之前使用默认日志级别的测试，
> 减少了日志输出带来的 I/O 和 CPU 开销，因此 QPS 数据可能高于之前的测试。
> 对比不同测试报告时需注意此差异。

### 2 线程测试矩阵 (worker_threads=2, reuseport=2)

| 场景 | doggy-dns cpu_affinity | dnsperf taskset | 物理核分配 |
|----|:-:|:-:|---|
| A: Baseline | 无 | 无 | 全部 8 vCPU 共享 |
| B: Isolated | [0, 2] (Core 0,1) | 4,6 (Core 2,3) | 完全隔离 |
| C: Contention | [0, 2] (Core 0,1) | 0,2 (Core 0,1) | 同核竞争 |
| D: dnsperf-only pinned | 无 | 4,6 (Core 2,3) | 仅 dnsperf 受限 |

### 4 线程测试矩阵 (worker_threads=4, reuseport=4)

| 场景 | doggy-dns cpu_affinity | dnsperf taskset | 物理核分配 |
|---|:-:|:-:|------|
| E: Baseline | 无 | 无 | 全部 8 vCPU 共享 |
| F: Isolated | [0, 2, 4, 6] (Core 0-3) | 1,3,5,7 (hyperthreads) | 物理核隔离 |
| G: Contention | [0, 2, 4, 6] (Core 0-3) | 0,2,4,6 (Core 0-3) | 同核竞争 |

## 测试结果

### 2 线程 (worker_threads=2, reuseport_listen_num=2)

| 场景 | 客户端 | Median QPS | Δ vs Baseline | Avg Lat (ms) | Max Lat (ms) |
|----|:---:|-------:|:------:|-------:|-----:|
| A: Baseline | 8 | 172,707 | — | 0.543 | 62.99 |
| A: Baseline | 32 | 186,264 | — | 0.495 | 62.12 |
| **B: Isolated** | **8** | **178,712** | **+3.5%** | 0.521 | 123.08 |
| **B: Isolated** | **32** | **188,418** | **+1.2%** | 0.480 | 98.22 |
| C: Contention | 8 | 135,206 | **-21.7%** | 0.625 | 45.36 |
| C: Contention | 32 | 130,605 | **-29.9%** | 0.652 | 94.81 |
| D: dnsperf-only pinned | 8 | 162,837 | -5.7% | 0.582 | 62.50 |
| D: dnsperf-only pinned | 32 | 175,005 | -6.0% | 0.513 | 93.55 |

### 4 线程 (worker_threads=4, reuseport_listen_num=4)
| 场景 | 客户端 | Median QPS | Δ vs Baseline | Avg Lat (ms) | Max Lat (ms) |
|----|:--:|-----:|:---:|----:|---:|
| E: Baseline | 8 | 293,736 | — | 0.289 | 111.28 |
| E: Baseline | 32 | 298,296 | — | 0.270 | 3.02 |
| **F: Isolated** | **8** | **307,194** | **+4.6%** | 0.271 | 48.39 |
| **F: Isolated** | **32** | **309,506** | **+3.8%** | 0.270 | 106.12 |
| G: Contention | 8 | 225,621 | **-23.2%** | 0.349 | 90.78 |
| G: Contention | 32 | 215,606 | **-27.7%** | 0.362 | 62.00 |

### 丢包率

所有测试配置下丢包率均为 **0%**。

## 分析

### 1. CPU 竞争是主要干扰因素
当 dnsperf 与 doggy-dns 共享相同 CPU 核心时（场景 C/G）：
- 2 线程 QPS 下降 **21.7% ~ 29.9%**
- 4 线程 QPS 下降 **23.2% ~ 27.7%**
- 平均延迟上升 **14% ~ 34%**
这解释了初次测试中"绑核降低性能"的假象 —— 实际是 dnsperf 被挤到
相同核心导致的竞争，而非绑核本身的问题。

### 2. 隔离后绑核带来正向收益

消除竞争后（场景 B/F），绑核**提升**了性能：
- 2 线程：QPS +1.2% ~ +3.5%（幅度较小，可能在误差范围内）
- **4 线程：QPS +3.8% ~ +4.6%**（明显正向）

4 线程收益更显著的原因：
- 线程数越多，跨核迁移的 cache 失效成本越高
- 绑核消除了 work-stealing 带来的不必要任务迁移
- 每个物理核独占一个 worker，cache 局部性最大化

### 3. 2 线程 vs 4 线程的差异

| 对比维度 | 2 线程 | 4 线程 |
|-----|:---:|:---:|
| 隔离后 QPS 提升 | +1.2% ~ +3.5% | +3.8% ~ +4.6% |
| 竞争导致 QPS 下降 | -21.7% ~ -29.9% | -23.2% ~ -27.7% |
| 绑核收益显著性 | 较低（可能被噪声淹没） | **明确正向** |

2 线程时，Tokio 调度器的 work-stealing 开销较小，绑核收益有限。
4 线程时，调度开销增大，绑核的 cache 局部性优势开始显现。

### 4. 仅限制 dnsperf 也会降低性能

场景 D 中，doggy-dns 自由调度但 dnsperf 被限制在 2 个核心：
- QPS 下降 5.7% ~ 6.0%

说明 dnsperf 本身也需要足够的 CPU 资源。在高 QPS 压测中，
dnsperf 的包构造、发送、响应解析同样消耗 CPU。

### 5. 物理核隔离 vs 超线程隔离

4 线程隔离场景（F）中：
- doggy-dns 使用 vCPU 0,2,4,6（每个物理核的一个超线程）
- dnsperf 使用 vCPU 1,3,5,7（每个物理核的另一个超线程）

虽然共享物理核的 L1/L2 cache，但避免了同核执行单元竞争。
如果要完全消除超线程干扰，可使用 `isolcpus` + `taskset` 将
两个进程分配到**完全不同的物理核**。

## 结论

| 结论 | 说明 |
|----|-----|
| **绑核本身是正向优化** | 隔离后 4 线程 QPS +3.8% ~ +4.6% |
| **CPU 竞争是严重干扰** | 同核竞争导致 QPS -21% ~ -30% |
| **初次测试结论有误** | "绑核降低性能" 实际是竞争导致的假象 |
| **压测必须隔离** | dnsperf 与 doggy-dns 应分配到不同 CPU 核心 |
| **推荐配置** | `worker_threads = reuseport_listen_num = 物理核数`，配合 `cpu_affinity` |

## 配置建议
### 生产环境（推荐）

```toml
[server]
worker_threads = 4          # 物理核数
reuseport_listen_num = 4    # 与 worker_threads 相同
cpu_affinity = [0, 2, 4, 6] # 每个 worker 绑定到一个物理核
```

### 压测环境

```bash
# doggy-dns 绑定物理核 0-3 的各一个超线程
doggy-dns config.toml &

# dnsperf 绑定到另一组超线程
taskset -c 1,3,5,7 dnsperf -s 127.0.0.1 -p 15353 -d queries.txt -l 30 -c 32
```

## 不适用绑核的场景

| 场景 | 原因 |
|---|---|
| worker_threads = 1 | 单线程无需绑核，调度器无迁移 |
| forward 插件 + 低 QPS | I/O 等待为主，CPU 不是瓶颈 |
| 容器资源受限 | cgroup 已限制 CPU 集合，绑核无额外收益 |

## 代码实现

新增 `cpu_affinity` 配置项：

```toml
[server]
worker_threads = 4
reuseport_listen_num = 4
cpu_affinity = [0, 2, 4, 6]  # 可选，绑定 worker 到指定 CPU 核心
```

实现使用 `core_affinity` crate，通过 tokio 的 `on_thread_start` 钩子
在每个 worker 线程启动时设置 CPU 亲和性。

---
*报告生成时间: 2026-11-02 18:45 CST*  
*所有数据均为实测，每组配置运行 30 秒 × 3 次取中位数，无外推/估算*

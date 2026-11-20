//! DNS runtime: the worker runtime that binds and serves the DNS sockets.
//!
//! Binding and serving must both happen on this runtime: hickory hands each
//! registered socket's accept/request tasks to whatever runtime is current at
//! registration (`Server::register_socket`), so the server future is spawned
//! here rather than built elsewhere and awaited here.

use anyhow::{Context, Result};
use doggy_dns_core::config::DoggyDnsConfig;
use doggy_dns_core::handler::DoggyDnsHandler;
use doggy_dns_core::server::build_server;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot, watch};

/// Prefix of every DNS worker thread name. The same string names the thread
/// and parses the affinity index back out of it.
const WORKER_THREAD_PREFIX: &str = "doggy-dns-worker-";

/// Management-side handles to the running DNS server task.
pub(crate) struct ServerHandle {
    pub(crate) stop: watch::Sender<bool>,
    pub(crate) done: mpsc::Receiver<Result<()>>,
}

/// Build the DNS runtime. Its workers are named `doggy-dns-worker-{id}` and,
/// when `cpu_affinity` is configured, pinned by that id.
pub(crate) fn build_runtime(config: &DoggyDnsConfig) -> Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    let next_worker = AtomicUsize::new(0);
    builder
        .enable_all()
        .thread_name_fn(move || {
            let id = next_worker.fetch_add(1, Ordering::Relaxed);
            format!("{WORKER_THREAD_PREFIX}{id}")
        })
        .worker_threads(config.server.worker_threads);

    if let Some(affinity) = &config.server.cpu_affinity {
        configure_cpu_affinity(&mut builder, affinity)?;
    }

    let runtime = builder.build().context("failed to build the dns runtime")?;
    tracing::info!(
        worker_threads = config.server.worker_threads,
        "dns runtime built"
    );
    Ok(runtime)
}

/// Start the DNS server on the runtime and wait until every socket is bound,
/// so a bind failure is still a startup error.
pub(crate) async fn start_server(
    dns: tokio::runtime::Handle,
    config: DoggyDnsConfig,
    handler: DoggyDnsHandler,
) -> Result<ServerHandle> {
    let (started_tx, started_rx) = oneshot::channel::<Result<()>>();
    let (done_tx, done_rx) = mpsc::channel::<Result<()>>(1);
    let (stop_tx, stop_rx) = watch::channel(false);

    dns.spawn(serve(config, handler, started_tx, done_tx, stop_rx));

    match started_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.context("failed to start the dns server")),
        Err(_) => anyhow::bail!("dns server task exited before the server was built"),
    }

    Ok(ServerHandle {
        stop: stop_tx,
        done: done_rx,
    })
}

async fn serve(
    config: DoggyDnsConfig,
    handler: DoggyDnsHandler,
    started: oneshot::Sender<Result<()>>,
    done: mpsc::Sender<Result<()>>,
    mut stop: watch::Receiver<bool>,
) {
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
            ServeExit::Finished(result.map_err(anyhow::Error::from))
        }
        _ = wait_for_stop_request(&mut stop) => ServeExit::StopRequested,
    };

    let result = match exit {
        ServeExit::Finished(result) => result,
        ServeExit::StopRequested => server
            .shutdown_gracefully()
            .await
            .map_err(anyhow::Error::from),
    };
    let _ = done.send(result).await;
}

enum ServeExit {
    Finished(Result<()>),
    StopRequested,
}

/// Resolve when management asks the server to stop. A dropped sender means
/// management is gone, which is also a reason to stop serving.
async fn wait_for_stop_request(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|requested| *requested).await;
}

/// Configure CPU affinity for the DNS runtime's worker threads.
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
        .and_then(|name| name.strip_prefix(WORKER_THREAD_PREFIX))
        .and_then(|index| index.parse().ok())
}

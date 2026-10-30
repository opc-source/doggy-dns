# Deployment

How to build, publish and run doggy-dns as a service: container images, the
Kubernetes manifests in `examples/deploy/`, and what the two image variants
actually differ in.

Configuration keys are documented in
[`configuration.md`](configuration.md); the XDP fast path itself, including how
it is compiled into the binary, is in [`xdp.md`](xdp.md).

## The two images

Both are the same Rust binary on the same Debian 12 slim root filesystem. They
differ in one cargo feature and one user:

| | `:main` | `:main-xdp` |
|---|---|---|
| build | `--target runtime` | `--target runtime-xdp --build-arg FEATURES=xdp` |
| fast path | not compiled in | eBPF program embedded in the binary |
| runs as | uid 65532 | root |
| host access needed | bind port 53 (`NET_BIND_SERVICE`) | `BPF` + `PERFMON` + `NET_ADMIN` + `NET_BIND_SERVICE`, and the host's bpffs |
| reads `[xdp]` | ignores it | acts on it |

The runtime base is glibc Debian rather than a musl or distroless image because
`aws-lc-sys` and `ring` — pulled in by rustls for DoT and DoH — are built with
`cmake` and a C toolchain and link against glibc at run time.

```
        :main (userspace)                    :main-xdp (fast path)

 client ──UDP/TCP :53──▶ doggy-dns           NIC ──▶ XDP program (kernel)
                          │                                │  hit: replay reply,
                          ▼                                │  redirect, no copy
                    authority chain                        ▼
                    (nacos/native/forward)           miss: map → :53 socket
                          ▼                                      │
                      response                                   ▼
                                                          doggy-dns (root)
```

With the fast path on, the answer a client gets on a hit was produced by an
earlier request: the kernel replays the payload pinned in the `DNS_CACHE` map. A
cold name goes up to userspace, is resolved by the authority chain, and its
answer is written back into that map.

## Building an image

`Dockerfile` takes both variants out of one file:

```bash
# userspace only. `--target` is required: without it the build stops at the
# last stage, which is the root XDP image.
docker build --target runtime -t doggy-dns:local .

# with the XDP fast path
docker build --build-arg FEATURES=xdp --target runtime-xdp -t doggy-dns:local-xdp .
```

The XDP stage installs a pinned `bpf-linker` and the nightly toolchain inside the
build, so the eBPF object is compiled by the image build itself — nothing has to
be built on the host first.

The registry paths are ARGs so a build from a network where the defaults are slow
or unreachable can be redirected without editing the file:

| ARG | Default | Example for a mirrored build |
|-----|---------|------------------------------|
| `BUILDER_IMAGE` | `docker.io/library/rust:1-slim-bookworm` | `docker.m.daocloud.io/library/rust:1-slim-bookworm` |
| `RUNTIME_IMAGE` | `docker.io/library/debian:12-slim` | `docker.m.daocloud.io/library/debian:12-slim` |
| `APT_MIRROR` | empty (the base image's own mirror) | `mirrors.ustc.edu.cn` |
| `CARGO_MIRROR_INDEX` | empty (crates.io) | `sparse+https://rsproxy.cn/index/` |
| `GHPROXY` | empty | `https://gh-proxy.com/` |
| `RUSTUP_MIRROR` | empty (`static.rust-lang.org`) | `https://rsproxy.cn` |
| `CARGO_EXTRA_ARGS` | empty | see below |

`RUSTUP_MIRROR` matters more than it looks: both workspaces pin a toolchain in
`rust-toolchain.toml` (`stable` for the binary, `nightly` + `rust-src` for the
eBPF program), so rustup downloads one before compiling anything. In a local
podman build that download alone measured 24 minutes before the first `rustc`
appeared. Pointed at a mirror it takes seconds. CI leaves it empty and fetches
from the default host.

`CARGO_EXTRA_ARGS` exists because the workspace's `profile.release` uses fat LTO
and a single codegen unit. That is what makes the binary fast, and what makes it
need more memory than a 2 GiB VM has:

```bash
--build-arg CARGO_EXTRA_ARGS='--config profile.release.lto=false -j 1'
```

The image then contains the same code linked less aggressively. CI leaves
`CARGO_EXTRA_ARGS` empty and builds on native runners, where the real profile
fits.

## Publishing

`.github/workflows/image.yml` publishes to `ghcr.io` on every push to `main`, and
on `workflow_dispatch`. Nothing is published from a feature branch: the workflow
is `push: branches: [main]` only.

For each push it builds four images — userspace and XDP, each on amd64 and on
arm64 — smoke-tests each one, pushes them under a temporary
`tmp-<sha>-<variant>-<arch>` tag, and then joins each pair into a multi-arch
manifest list:

```
ghcr.io/<org>/doggy-dns:main          ← amd64 + arm64, userspace
ghcr.io/<org>/doggy-dns:main-xdp      ← amd64 + arm64, XDP
ghcr.io/<org>/doggy-dns:sha-<7>       ← same images, pinned to one commit
ghcr.io/<org>/doggy-dns:sha-<7>-xdp
```

The `tmp-*` tags are not cleaned up afterwards — removing a tag needs the
registry's package API, and the manifest list addresses the images by digest, so
they cost nothing but a line in the package's tag list. Each push adds four.

Three deliberate properties:

- **No `:latest`.** A node's resolver should report which build it is running, and
  a rolling `:main` already says that.
- **Smoke before push.** An image that fails the smoke test never reaches a
  rolling tag, so a bad merge cannot silently spread to every node on its next
  pull. CI runs the smoke without `--xdp`: attaching a program needs a writable
  bpffs and an XDP-capable NIC, and a runner that happens to lack either would
  block every publish. The `xdp` variant still gets its userspace path checked
  there, and the kernel behaviour is covered by the root-only integration tests.
- **The rolling tag is a moving target**, which is why `imagePullPolicy: Always`
  appears in the sample manifests and why `:sha-*` exists. Pin a `:sha-*` tag if
  you would rather choose the moment a node updates.

The first publish also needs a one-time action in the organization's UI: connect
the repository to the package with write access. Until that is done the registry
rejects the push and CI reports it as a `denied` error; no workflow permission can
fix it from inside the run.

## Verifying an image

`scripts/image_smoke.py` starts an image and checks the things a manifest cannot
express: that the image declares the user it is meant to run as (`65532:65532`
userspace, root with XDP), that the binary with no arguments looks for
`/app/config/doggy-dns.toml` — the path the image bakes its default at — that the
control plane answers, that a raw UDP **and** TCP query is answered, and — with
`--xdp` — that the pinned `DNS_CACHE` map survives the container being replaced.

The smoke mounts **its own** config over that path, so the baked default's
*contents* are not what answers there; that file is covered instead by
`container_default_config_loads_with_no_environment` in `doggy-dns-basic`'s
`config_test`, which loads and validates it exactly as shipped — no environment
variables, no volumes.

```bash
python3 scripts/image_smoke.py --image ghcr.io/<org>/doggy-dns:main
python3 scripts/image_smoke.py --image ghcr.io/<org>/doggy-dns:main-xdp --xdp
```

The script serves its own one-record forward upstream on loopback, so a run needs
no outbound internet, and the expected answer is a fixed TEST-NET address. It also
means the container has to be able to reach the script's port, so it always runs
with `--network host`. On macOS with podman, that is the VM's loopback rather than
your laptop's, so run the script inside the machine:

```bash
podman machine ssh 'cat > /tmp/image_smoke.py' < scripts/image_smoke.py
podman machine ssh 'python3 /tmp/image_smoke.py --runtime podman --image <image>'
```

`--xdp` additionally requires bpffs mounted on the runtime host and an interface
that accepts an XDP program. The script attaches to `lo` unless `--interface`
names another device, because a real NIC may be unable to take an XDP program
(the podman VM's virtio device is one that cannot) while loopback takes one
wherever the fast path runs at all — and the queries the smoke sends go over
loopback. The reuse step proves the map, not the process, is what answered: the
script closes its upstream, restarts the container pointed at a dead port, and
expects the cached name to still be answered.

## Running it outside Kubernetes

The image needs one file and, for port 53, one capability:

```bash
docker run -d --name doggy-dns \
  -p 127.0.0.1:5353:5353/udp -p 127.0.0.1:5353:5353/tcp \
  -v "$PWD/my-config.toml:/app/config/doggy-dns.toml:ro" \
  ghcr.io/<org>/doggy-dns:main
```

Pass a path as the first argument to read a config from anywhere else
(`docker run <image> /elsewhere.toml`). Any `${ENV_VAR}` in the config is expanded
by the server at startup and a missing variable is fatal, so pass the values with
`-e`.

To bind port 53 as the image's unprivileged user, add the capability instead of
running as root:

```bash
docker run -d --cap-drop ALL --cap-add NET_BIND_SERVICE -p 53:53/udp -p 53:53/tcp ...
```

The XDP variant needs `--privileged`-adjacent access that is easier to express as
the capabilities in `examples/deploy/daemonset-xdp.yaml`, the host's bpffs
mounted in, and the host's network namespace. It runs as root, but still passes
`NET_BIND_SERVICE`: `--cap-drop ALL` strips root's implicit right to bind a
privileged port, so port 53 needs that capability back.

```bash
docker run -d --network host --user 0:0 \
  --cap-drop ALL \
  --cap-add BPF --cap-add PERFMON --cap-add NET_ADMIN --cap-add NET_BIND_SERVICE \
  -v /sys/fs/bpf:/sys/fs/bpf \
  -v "$PWD/xdp-config.toml:/app/config/doggy-dns.toml:ro" \
  ghcr.io/<org>/doggy-dns:main-xdp
```

## Kubernetes

`examples/deploy/` holds the bare manifests — three ConfigMaps (the userspace
config, the XDP config, and the shared env vars), and one DaemonSet per variant —
with the apply order, the bpffs prerequisite and the config-refresh caveat
documented in [`examples/deploy/README.md`](../examples/deploy/README.md).

They are DaemonSets because doggy-dns answers in the **node's** network namespace
(`hostNetwork: true`): a per-cluster service ClusterIP would put a resolver in
front of the kubelet's own DNS, and a node that cannot resolve anything cannot
pull the image that fixes it. `priorityClassName: system-cluster-critical` plus
`tolerations: [operator: Exists]` keep it schedulable on control-plane and
tainted nodes.

Nothing here wires the cluster to use it. Point kubelet, node-local DNS or
CoreDNS's upstream at the DaemonSet's address as your cluster's DNS setup
requires; the manifests only run the server.

## Upgrading and rolling back

Because `:main` is overwritten by the next merge, the upgrade is a rollout:

```bash
kubectl -n doggy-dns rollout restart daemonset/doggy-dns       # picks up current :main
kubectl -n doggy-dns rollout undo daemonset/doggy-dns          # back to the previous pod spec
```

`rollout undo` returns to the previous *spec*, and with a rolling tag the previous
spec still names the same moving tag — so if you want a rollback that actually
pulls an older image, deploy `:sha-<7>` explicitly. That is the reason the
per-commit tags are published alongside the rolling ones.

For the XDP variant one more thing survives a rollout: the pinned cache map. A new
pod finds the map its predecessor pinned and keeps serving from it, which is what
you want for the answers already cached, and what you do *not* want if the new
release changed the cached entry layout. That case is handled by the version
suffix in the pin path (see [`xdp.md`](xdp.md)); a layout change makes the next
start a cold start on a fresh path rather than mixing formats.

## Troubleshooting

- **`ImagePullBackOff` with `denied`** — the package is still private, or the node
  has no pull secret. ghcr packages are private until the repository is linked or
  the package's visibility is set to public:
  `kubectl -n doggy-dns create secret docker-registry ghcr --docker-server=ghcr.io
  --docker-username=<github-user> --docker-password=<token with read:packages>`,
  then `imagePullSecrets: [name: ghcr]`.
- **Pod starts and answers, but the `[xdp]` block did nothing** — the fast path is
  compiled out (`:main` instead of `:main-xdp`), or the container is not root, or
  bpffs is not mounted at the pin path. The server logs a warning at startup and
  keeps serving from userspace; look for the XDP lines in `kubectl logs`.
- **`bpffs` and pin errors mentioning `Operation not permitted`** — bpffs refuses
  object names containing a `.`. `pin_path` must be dot-free, and `validate_config`
  rejects a dotted one at startup rather than failing when the program attaches.
- **Config edits not taking effect** — a `subPath` mount is bound at mount time.
  `kubectl rollout restart` after editing a ConfigMap.
- **Changing `cache_size` has no effect** — the capacity is fixed when the pinned
  map is created; editing the config changes nothing until the pin file is
  removed. The server warns at startup when the pinned map's capacity and the
  config disagree, and keeps running on the pinned map.
- **DoT/DoH not reachable** — the manifests publish 53 and the control plane only.
  `[server.tls]` binds 853 and `[server.https]` binds 443; add those container
  ports and host ports yourself if you turn them on.

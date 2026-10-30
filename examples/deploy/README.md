# Deployment examples

Bare manifests for running the published images. There is no Helm chart and no
operator here on purpose: these are the smallest thing that works, and every
field that a real cluster would need to change is visible in the file.

| File | What it is |
|------|------------|
| `container-default.toml` | The config baked into both images at `/app/config/doggy-dns.toml`. |
| `configmap.yaml` | Three ConfigMaps: the userspace config, the XDP config, and the env vars. |
| `daemonset.yaml` | The userspace resolver, one pod per node, on port 53 in the node's netns. |
| `daemonset-xdp.yaml` | The same with the kernel fast path, plus the capabilities and bpffs mount it needs. |

The image names point at `ghcr.io/opc-source/doggy-dns`; change them if you
mirror the images into your own registry. See
[`docs/deployment.md`](../../docs/deployment.md) for how the images are built
and published, and for running them outside Kubernetes.

## Apply order

```bash
kubectl create namespace doggy-dns
kubectl apply -f examples/deploy/configmap.yaml
kubectl apply -f examples/deploy/daemonset.yaml        # userspace
# or, on nodes that have bpffs mounted (see the header of daemonset-xdp.yaml):
kubectl apply -f examples/deploy/daemonset-xdp.yaml
```

Both DaemonSets can run at once only if they use different ports; a node has one
port 53. Pick one variant per cluster.

## Which image

`:main` is the userspace resolver. It drops every capability except
`NET_BIND_SERVICE` (needed to bind port 53 as uid 65532), runs as uid 65532, and
needs nothing else from the host.

`:main-xdp` is the same binary with the `xdp` feature compiled in, running as
root with `BPF` + `PERFMON` + `NET_ADMIN` + `NET_BIND_SERVICE`, because loading a
program and setting an interface's XDP fd are privileged, and dropping all
capabilities takes away root's own right to bind port 53. It is the only variant
that reads the `[xdp]` block; with `enabled = false` it behaves exactly like
`:main`.

## Config

The image's baked default (`container-default.toml`) is forward-only to
`8.8.8.8` / `1.1.1.1`, with observability off and the control plane on. It exists
so a container answers DNS with zero configuration; the repository's
`config/doggy-dns.toml` is a development sample and cannot even start there,
because its OTLP block needs `${OPENOBSERVE_PASSWORD}` to be set.

The DaemonSets mount a ConfigMap over that path, so the config in the cluster is
the one in `configmap.yaml`. Two consequences worth knowing:

- Any `${ENV_VAR}` in the config is expanded by the server at startup, and a
  missing variable is fatal. `configmap.yaml` keeps the expansion in one place:
  the `doggy-dns-env` ConfigMap is loaded with `envFrom`, so `DOGGY_NACOS_ADDR`
  and `DOGGY_UPSTREAM` are defined for both variants.
- A `subPath` mount is a bind of the ConfigMap's content at mount time, so
  editing the ConfigMap does **not** reach a running pod. Refresh with:

  ```bash
  kubectl -n doggy-dns rollout restart daemonset/doggy-dns
  ```

## XDP notes

`daemonset-xdp.yaml` mounts the node's `/sys/fs/bpf` and does not create it;
mounting a filesystem needs `CAP_SYS_ADMIN` in the host's mount namespace, which
this pod deliberately does not have. The systemd unit in the file's header is the
prerequisite. Without bpffs the server still starts — the fast path logs a warning
and the pod serves from userspace.

The cache map is pinned at `<pin_path>_v1`, and a restart reuses that map instead
of creating an empty one, so answers already in the cache survive the pod being
replaced. Three rules follow from that:

- `pin_path` must not contain a `.`: bpffs refuses to pin an object whose name
  has a dot, and the server rejects such a config at startup rather than failing
  when it attaches.
- `cache_size` is fixed when the map is first created. Changing it later changes
  nothing until the pin file is removed; the server warns at startup when the
  pinned map and the config disagree, and keeps running on the pinned map.
- The version suffix is the cache layout version. Bumping it is how a release
  forces a cold start after an incompatible change to the cached entry format.

## Verifying an image before you deploy it

`scripts/image_smoke.py` runs a built image and checks that it declares the user
it is meant to run as (65532 for `:main`, root for `:main-xdp`), starts from its
baked config path (its own config is mounted over that file for the run; the
shipped `container-default.toml` is checked by
`container_default_config_loads_with_no_environment` in `doggy-dns-basic`), serves
`/healthz` and `/readyz`, answers UDP and TCP queries,
and — with `--xdp` — that the pinned cache survives a container replacement. It
needs to run on the machine that hosts the container runtime:

```bash
python3 scripts/image_smoke.py --image ghcr.io/<org>/doggy-dns:main
python3 scripts/image_smoke.py --image ghcr.io/<org>/doggy-dns:main-xdp --xdp

# macOS + podman: the container shares the VM's network namespace, not your
# laptop's, so run the script inside the VM.
podman machine ssh --username root 'cat > /tmp/image_smoke.py' < scripts/image_smoke.py
podman machine ssh --username root \
  'python3 /tmp/image_smoke.py --runtime podman --image <org>/doggy-dns:main-xdp --xdp'
```

CI runs this script for both images but never with `--xdp`: attaching a program
needs a writable bpffs and an XDP-capable NIC, and a GitHub runner that happens
to lack either would block every publish. The `:main-xdp` image still gets its
userspace path checked there, because without an `[xdp]` block in the config the
fast path stays dormant. The kernel reuse behaviour is covered by the root-only
`xdp_integration_test` cases.

## Registry

Images are published by `.github/workflows/image.yml` on every push to `main`, as
multi-arch (amd64 + arm64) manifest lists:

- `:main`, `:main-xdp` — rolling, overwritten by the next merge.
- `:sha-<7>`, `:sha-<7>-xdp` — the same images pinned to one commit.

There is deliberately no `:latest`: a node DNS resolver should say which build it
is running. `imagePullPolicy: Always` on the rolling tags is what makes that
trade-off explicit — pin a `:sha-*` tag if you would rather control the rollout.

The first publish also needs a one-time step in the organization's UI: ghcr shows
a private package until the repository is linked as a "Connected repository" with
write access, and a workflow run cannot fix that itself. Until then the push in CI
fails with `denied`.

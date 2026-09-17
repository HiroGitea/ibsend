# Containers and Kubernetes

ibsend can run in Docker and Kubernetes on hosts with RDMA adapters. The
container image contains the CLI only. This document covers the image, the
runtime settings RDMA needs, Kubernetes deployment, the `kubectl ibsend`
plugin and the machine-readable interface used by scripts.

For general usage, see the [README](../README.md).

- [Image](#image)
- [Runtime requirements](#runtime-requirements)
- [Docker](#docker)
- [Kubernetes](#kubernetes)
- [kubectl plugin](#kubectl-plugin)
- [Scripting interface](#scripting-interface)
- [Example workloads](#example-workloads)
- [Security](#security)
- [Troubleshooting](#troubleshooting)

## Image

No prebuilt image is published. Build it from the repository root and push it
to a registry your nodes can pull from:

```sh
docker build -t registry.example.com/ibsend:0.1.0 .
docker push registry.example.com/ibsend:0.1.0
```

The image is based on Debian 12 and includes the `ibverbs-providers` user-space
drivers.

| Item | Value |
|---|---|
| Entrypoint | `tini -- ibsend`, so any arguments form an `ibsend` command. |
| Default command | `daemon --out /data --ready-file /run/ibsend/ready` |
| User | UID and GID `10001` |
| Receive directory | `/data` |
| Ready file | `/run/ibsend/ready` |

`tini` forwards `SIGTERM` to ibsend, so containers stop promptly. Interrupted
transfers keep their `.part` files and resume on the next send.

## Runtime requirements

| Requirement | Reason | Docker | Kubernetes |
|---|---|---|---|
| IPoIB interface in the container's network namespace | `rdma_cm` resolves addresses through it; discovery scans its subnet | `--network host` | `hostNetwork: true`, or a Multus IPoIB attachment |
| RDMA devices | Verbs and `rdma_cm` use `/dev/infiniband` | `--device /dev/infiniband` | An RDMA device plugin resource |
| `IPC_LOCK` capability | Registered memory must stay pinned | `--cap-add IPC_LOCK` | `securityContext.capabilities.add` |
| No `no_new_privs` | The non-root user receives `IPC_LOCK` through a file capability | Do not use `--security-opt no-new-privileges` | `allowPrivilegeEscalation: true` |
| Memory limit | Sets the size of the receive pool | `--memory` | `resources.limits.memory` |

RDMA traffic bypasses the kernel network stack. `rdma_cm` port numbers are not
TCP ports, so Kubernetes Services, `kube-proxy` and network policies do not
apply to ibsend traffic. Peers connect directly to a node or pod address on the
IPoIB network.

### Memory locking

Without `CAP_IPC_LOCK`, the kernel's default limit restricts registered memory
to about 8 MiB, and throughput drops sharply. `ibsend authorize` does not work
inside containers, so the capability must come from the container runtime.

The image contains two copies of the binary. `ibsend-ipc-lock` has the
`cap_ipc_lock+ep` file capability. The kernel refuses to execute it unless
`IPC_LOCK` is in the container's capability bounding set. The entrypoint checks
the bounding set first. It starts the capable copy when `IPC_LOCK` is present
and falls back to the plain copy otherwise. When the capability is missing,
ibsend prints the runtime setting that is needed.

With `no_new_privs` set, the kernel ignores file capabilities and the non-root
user runs without `IPC_LOCK`. Containers running as root with `IPC_LOCK` added
do not depend on the file capability.

### Memory limits

Pinned memory counts toward the container's memory limit and cannot be
reclaimed. ibsend reads the cgroup limit (v1 and v2) and sizes the receive pool
from the memory that remains available, excluding reclaimable page cache. With
automatic sizing, the pool uses about one third of that memory, between 256 MiB
and 16 GiB. An explicit `--pool`, and the 256 MiB minimum, are capped at three
quarters of the remaining memory. The sender limits its local buffer to half of
the remaining memory.

Set a memory limit that matches the pool you want. For example, an 8 GiB limit
gives a receive pool of about 2.7 GiB.

## Docker

Start a receiver that stores files in `./inbox`:

```sh
mkdir -p inbox && sudo chown 10001:10001 inbox
docker run -d --name ibsend \
  --network host --device /dev/infiniband \
  --cap-drop ALL --cap-add IPC_LOCK \
  --memory 8g \
  -v "$PWD/inbox:/data" \
  ibsend daemon --out /data --name nas
```

Send files from another host or container:

```sh
docker run --rm --network host --device /dev/infiniband \
  --cap-drop ALL --cap-add IPC_LOCK \
  -v "$PWD/dataset:/src/dataset:ro" \
  ibsend send nas /src/dataset
```

To write files as your own user, add `--user "$(id -u):$(id -g)"`.

[`deploy/docker/compose.yaml`](../deploy/docker/compose.yaml) runs the same
receiver with a read-only root filesystem and a health check based on the ready
file:

```sh
docker compose -f deploy/docker/compose.yaml up -d
```

With host networking, every container on a host shares the same `rdma_cm` port
space. A second receiver on the same host needs a different `--port`.

## Kubernetes

### Prerequisites

1. **RDMA devices.** Deploy the
   [k8s-rdma-shared-dev-plugin](https://github.com/Mellanox/k8s-rdma-shared-dev-plugin).
   [`prerequisites/rdma-shared-device-plugin.yaml`](../deploy/kubernetes/prerequisites/rdma-shared-device-plugin.yaml)
   contains a configuration that exposes Mellanox InfiniBand adapters as
   `rdma/hca_shared_devices_a`.
2. **Pod network.** Choose one of the network modes below. Multus mode
   additionally needs [Multus](https://github.com/k8snetworkplumbingwg/multus-cni),
   [ipoib-cni](https://github.com/Mellanox/ipoib-cni) and an IPAM plugin such as
   [whereabouts](https://github.com/k8snetworkplumbingwg/whereabouts); see
   [`prerequisites/ipoib-network.yaml`](../deploy/kubernetes/prerequisites/ipoib-network.yaml).
3. **Namespace.** Pods need `IPC_LOCK`, and host mode needs `hostNetwork`.
   The Pod Security Admission `baseline` and `restricted` levels allow neither,
   so run ibsend in a namespace labeled `privileged`:

   ```sh
   kubectl apply -f deploy/kubernetes/namespace.yaml
   ```

### Network modes

| | `host` (default) | `multus` |
|---|---|---|
| Pod address | The node's IPoIB address | A per-pod IPoIB address from IPAM |
| Discovery | Works without changes | Works when the attachment subnet has at most 4096 addresses |
| Receivers per node | One per port; temporary receivers use other ports | Any number |
| Isolation | Pod shares the node's network namespace | Pod has its own interface |

### Install with Helm

Label the nodes that have InfiniBand adapters, then install the chart:

```sh
kubectl label node gpu-1 gpu-2 gpu-3 example.com/infiniband=true
helm install ibsend deploy/helm/ibsend \
  --namespace ibsend \
  --set image.repository=registry.example.com/ibsend \
  --set image.tag=0.1.0 \
  --set resources.limits.memory=8Gi \
  --set-string 'nodeSelector.example\.com/infiniband=true'
```

The chart deploys a DaemonSet that runs `ibsend daemon` on each selected node.
Without a node selector, pods on nodes that lack the RDMA resource remain
pending. The daemon advertises the node name for discovery. Commonly changed
values:

| Value | Default | Description |
|---|---|---|
| `image.repository`, `image.tag` | `ibsend`, chart app version | Image to run. |
| `rdma.resourceName` | `rdma/hca_shared_devices_a` | Device plugin resource requested by each pod. |
| `network.mode` | `host` | `host` or `multus`. |
| `network.multus.networks` | empty | NetworkAttachmentDefinition for `multus` mode. |
| `storage.type` | `hostPath` | `hostPath`, `pvc` or `emptyDir`. |
| `storage.hostPath.path` | `/var/lib/ibsend/inbox` | Receive directory on each node. |
| `storage.hostPath.fixPermissions` | `true` | Change the directory's owner to the pod user at startup. |
| `resources.limits.memory` | unset | Sets the receive pool size. |
| `daemon.name` | node name | Name advertised during discovery. |
| `daemon.port` | `18515` | `rdma_cm` port. |
| `daemon.json` | `false` | Write JSON events to the log instead of text. |

See [`values.yaml`](../deploy/helm/ibsend/values.yaml) for all values.

Each pod's startup and readiness probes check the ready file. The file is
created after the receive pool has been registered, which can take several
seconds for large pools.

### Plain manifests

[`deploy/kubernetes`](../deploy/kubernetes) contains manifests rendered from
the chart with default values, for clusters that do not use Helm:

```sh
kubectl apply -f deploy/kubernetes/namespace.yaml
kubectl apply -f deploy/kubernetes/ibsend-hostnetwork.yaml
# or, with Multus:
kubectl apply -f deploy/kubernetes/ibsend-multus.yaml
```

Edit the image name and node selection before applying them.

## kubectl plugin

[`contrib/kubectl-ibsend`](../contrib/kubectl-ibsend) is a shell script that
runs transfers inside the cluster. It creates temporary sender and receiver
pods, streams the sender's output, returns its exit code and deletes the pods.
It requires only `kubectl`.

```sh
install -m 0755 contrib/kubectl-ibsend ~/.local/bin/
export IBSEND_IMAGE=registry.example.com/ibsend:0.1.0
```

List the daemons and their IPoIB addresses:

```sh
kubectl ibsend peers
```

Copy data:

```sh
# The contents of a PVC to the daemon on gpu-3, under datasets/imagenet
kubectl ibsend cp pvc/imagenet node/gpu-3:datasets/imagenet

# Between two PVCs
kubectl ibsend cp pvc/old-data pvc/new-data

# A directory on a node to a PVC
kubectl ibsend cp node/nas-1:/srv/export/models pvc/models
```

Directory sources send their contents. When the source PVC is already mounted
by a running pod, the sender is scheduled on the same node. Run
`kubectl ibsend help` for all options. `IBSEND_IMAGE`, `IBSEND_NETWORK` and
`IBSEND_RDMA_RESOURCE` set defaults for the corresponding options.

The plugin exits with the exit code of the ibsend sender or receiver, so it can
be used in scripts. With `--json`, it writes the transfer's JSON events with an
added `source` field (`sender` or `receiver`). `--dry-run` prints the pods it
would create. The plugin needs permission to create, delete and list pods, read
their logs and run `kubectl exec` in the namespaces it uses.

## Scripting interface

### JSON events

With `--json`, `daemon`, `recv`, `send` and `discover` write one JSON object
per line to standard output. Text messages are suppressed; warnings still go to
standard error. Event and field names are part of the stable interface.

| Event | Commands | Fields |
|---|---|---|
| `local` | `discover` | `iface`, `addr`, `mask` |
| `peer` | `discover` | `name`, `addr`, `note`, `inbox`, `local` |
| `ready` | `daemon`, `recv` | `bind`, `port`, `pool`, `name`, `inbox` |
| `started` | `daemon`, `recv`, `send` | `files`, `bytes`; `send` adds `peer` |
| `progress` | `daemon`, `recv`, `send` | `bytes`, `total`, `rate`, `elapsed`; receivers add `staged` |
| `done` | `daemon`, `recv`, `send` | `bytes`, `elapsed`, `rate`, `skipped`, `resumed`; receivers add `verified` and `bad`, `send` adds `inbox` |
| `failed` | `daemon`, `recv` | `message` (a connection or transfer failed; the daemon continues) |
| `error` | all | `code`, `message` (the command is exiting) |

Byte counts are integers, `rate` is bytes per second and `elapsed` is seconds.
`progress` events are written at most once per second.

```sh
ibsend discover --json | sed -n 's/.*"event":"peer".*"addr":"\([^"]*\)".*/\1/p'
```

### Ready file

`daemon` and `recv` accept `--ready-file <PATH>`. The file is removed at
startup and written once the receive pool is registered. It contains the
`ready` event, so scripts can read the listening address from it.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Failure |
| 2 | Invalid arguments |
| 3 | CRC32C mismatch in received files (`recv`) |
| 4 | No peer with the given name (`send`) |

### Output without a terminal

When standard error is not a terminal, progress is written as a new line every
five seconds instead of being updated in place. This keeps `docker logs` and
`kubectl logs` readable.

### Other options

- `--port <N>` selects the `rdma_cm` port for `daemon`, `recv`, `send` and
  `discover`.
- `send --prefix <DIR>` places files under a subdirectory of the receiver's
  output directory.
- `discover` and name lookup in `send` include receivers on the local host.

## Example workloads

[`deploy/kubernetes/examples`](../deploy/kubernetes/examples) contains:

| File | Workload |
|---|---|
| [`checkpoint-sidecar.yaml`](../deploy/kubernetes/examples/checkpoint-sidecar.yaml) | A training Job with a sidecar that sends each completed checkpoint to a storage node. |
| [`dataset-prewarm-job.yaml`](../deploy/kubernetes/examples/dataset-prewarm-job.yaml) | A Job that copies a dataset from a PVC to the daemon on every node. |
| [`pvc-migrate.yaml`](../deploy/kubernetes/examples/pvc-migrate.yaml) | A receiver pod and a sender Job that copy one PVC to another. |

The examples use host networking. Comments in each file describe the changes
for Multus.

## Security

ibsend does not encrypt or authenticate transfers. Anyone who can reach a
receiver on the RDMA network can write files to its receive directory. In a
cluster:

- Limit the IPoIB network to trusted nodes. Kubernetes network policies do not
  filter RDMA traffic.
- Run ibsend in a dedicated namespace. The `privileged` Pod Security level
  applies to the whole namespace.
- Use a dedicated receive directory or volume.
- Pods drop all capabilities except `IPC_LOCK` and use a read-only root
  filesystem.

## Troubleshooting

CLI messages are in Chinese. The messages below are followed by English
translations.

**`找不到 /dev/infiniband/rdma_cm`** (`/dev/infiniband/rdma_cm` not found).
The container has no RDMA devices. Add `--device /dev/infiniband`, or check
that the pod requests the device plugin resource. On a host, load the
`rdma_ucm` kernel module.

**`受 memlock 限制`** (limited by memlock). The process does not have
`IPC_LOCK`. Add the capability. If the message also mentions `no_new_privs`,
remove `no-new-privileges` or set `allowPrivilegeEscalation: true`.

**`找不到 IPoIB 接口`** (no IPoIB interface found). The network namespace has
no IPoIB interface. Use host networking or check the Multus attachment. For
RoCE, pass `--bind <address>`.

**Discovery finds nothing.** Check that the peers share an IPoIB subnet with at
most 4096 addresses and use the same `--port`. Otherwise, send to an IPv4
address.

**Pods stay pending.** Check that nodes report the RDMA resource with
`kubectl describe node`, and that `rdma.resourceName` matches it.

**`Permission denied` under `/data`.** The receive directory is not writable by
UID 10001. Enable `storage.hostPath.fixPermissions`, change the directory
owner, or run the pod as another user.

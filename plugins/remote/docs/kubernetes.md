# The main agent in a Kubernetes cluster

A generic example, in `examples/kubernetes/`, of the setup in [README.md](../README.md): one main agent that lives in a pod, workers that it starts in a second namespace, and your terminal attached from anywhere. It is for a cluster **you control**. The main agent holds your credentials (`auth.json`), so a cluster whose operator is someone else runs workers at most (an untrusted remote, which never gets a credential; see [trust.md](trust.md)). Every name in the manifests is a placeholder or a neutral default: replace `<agent-image>`, `<image>`, `<storage-class>`, `<object-store-cidr>`, `<object-store-port>` and `<api-server-cidr>`.

## What it creates

| File | Holds |
|---|---|
| `namespaces.yaml` | `mu-agent` (the main agent) and `mu-workers` (the worker pods) |
| `rbac.yaml` | ServiceAccount `mu-agent` and a Role in `mu-workers` only: `create`, `get`, `list`, `watch`, `delete` on pods and `create` on `pods/exec`. No secrets, no ClusterRole |
| `network-policies.yaml` | both namespaces refuse all ingress; `mu-workers` may reach DNS, the internet outside the private ranges and the object store; `mu-agent` may reach DNS, the internet and the API server |
| `storage.yaml` | the PVCs `auth`, `home` and `work` |
| `agent.yaml` | a StatefulSet of one running `mu agent --detach` as uid 1000, and the ConfigMap with its init script, mu's `config.toml` and `mu-remote`'s |
| `kustomization.yaml` | all of the above |

The agent container runs as a non-root user, with a read-only root file system, every capability dropped, no privilege escalation and the `RuntimeDefault` seccomp profile; `/tmp` is an `emptyDir`. Its init script starts the agent and follows its log (`kubectl logs`); a liveness probe on `mu agent list --json` restarts the container, and with it the agent, when the agent is gone. Workers keep `automountServiceAccountToken: false` (the pod template of the pod adapter sets it), so a worker has no API access at all.

## Deploy

1. Build two images from a checkout of this repository and push them to a registry the cluster can pull from. The images take a `mu` binary (a mu release's, of the version the machine that launches tasks runs: mu refuses a worker of another version) and `omp` for the agent image, from files in the build context; `mu-remote` is built from this repository. Use a new tag each time: a node that has a tag does not pull it again.

   ```sh
   cp /path/to/mu ./mu && cp /path/to/omp ./omp
   docker build --target worker -t <registry>/mu-remote-worker:<tag> .
   docker build --target agent  -t <registry>/mu-remote-agent:<tag>  .
   ```

   `worker` is what the pod adapter starts for a task (`<image>` in the agent's `mu-remote` config): git, `mu` and `mu-remote`, no language toolchain. Build your project's own image `FROM` it with the toolchain it needs (and a compiler cache, if you use one) and use that as `<image>`. `agent` is the worker plus omp (for `/login` and MCP sign-ins), `kubectl` and `jq`, `tini` as PID 1, and the pod adapter with its template in `/usr/local/lib/mu-remote/` (`<agent-image>` in `agent.yaml`). `--build-arg KUBECTL_VERSION=…` picks another kubectl, which may be one minor version off the API server's; `MU_FILE` and `OMP_FILE` name other files for `mu` and `omp`.
2. Choose a StorageClass that is **encrypted at rest**: the volumes hold repositories with their history, transcripts and `auth.json`. Set it as `<storage-class>` in `storage.yaml`.
3. Fill in the other placeholders. The API server's address is `kubectl get endpoints kubernetes -n default`; the object store is the compiler cache's endpoint (drop that egress rule and `sccache_secret_name` if you run none).
4. `kubectl apply -k examples/kubernetes/`.

The NetworkPolicies need a CNI that enforces them; without one they are ignored silently.

## The agent's own remote

The `mu-agent-init` ConfigMap carries two files the first start copies into place (when none exists; edit the real files afterwards). mu's `config.toml` in the profile home sends task subagents through the launcher:

```toml
[task]
launcher = ["mu-remote", "launch", "workers"]
launch = ["task"]
```

and `~/.config/mu-remote/config.toml` (on the `home` volume) names the pod adapter with the pod's in-cluster credentials (no `context`, so `kubectl` uses the service account), so the agent starts its own workers in `mu-workers`:

```toml
[remote.workers]
adapter = "/usr/local/lib/mu-remote/mu-remote-pod"
trusted = true                  # your cluster; this allows sccache_secret_name
[remote.workers.settings]
namespace = "mu-workers"
image = "<image>"
sccache_secret_name = "mu-sccache"   # optional; a Secret in mu-workers that you create
```

## Attach from your laptop

On the laptop, install the adapter (`mu-remote install-adapter pod`) and add a remote that points at the agent's namespace, with your kubeconfig as the credential:

```toml
[remote.cluster]
adapter = "pod"
trusted = true
[remote.cluster.settings]
context = "<kube-context>"
agent_namespace = "mu-agent"
namespace = "mu-workers"
```

`mu-remote cluster` lists the agents in `agent_namespace` (the pod carries the labels `app=mu-task`, `mu-kind=agent` that the adapter selects on) and joins the one that runs; `mu-remote cluster -c` joins the latest. You need `get`, `list` and `pods/exec` in `mu-agent` through your own kubeconfig. The connection is `kubectl exec -i POD -- mu-remote bridge ID`: the [encrypted channel](channel.md) ends in the pod, next to the agent. The adapter starts a second, separate agent pod there only when you ask for a new session and no agent can be found; to use only the StatefulSet's agent, attach with `-c` or `--resume`.

## Credentials

Nothing secret goes in a manifest, a Secret or a config file. Run `/login` (or `mu auth add`) inside the attached session: it runs in the pod and writes to the `auth` volume. mu keeps its `auth.json` in its profile home and replaces it by renaming a new file over it (a single-file mount cannot be renamed over), and omp keeps its own credentials in its agent directory, so the `auth` volume holds the agent's whole profile home (`auth.json`, sessions, memory) with the omp agent directory (`PI_CODING_AGENT_DIR`, set in `agent.yaml`) inside it, and it is mounted into the agent container only; the `home` volume holds the rest of the user's home (caches, git configuration, `mu-remote`'s config and its keys under `~/.local/state/mu-remote`) and `work` the repository checkouts. The profile home is the directory `profile` on the `auth` volume (`MU_HOME` in `agent.yaml`), not the mount point: a volume's root belongs to root, and mu needs to own its profile home (it sets it to mode 0700). The repository first reaches the pod as a bundle from your client (an initial `start --dir`); the pod holds no deploy key.

`mu-remote key` in the pod prints the bridge key's fingerprint; compare it with what the client shows when it pins the key for the first time (`kubectl exec -n mu-agent mu-agent-0 -- mu-remote key`). Keep the `home` volume: it holds the bridge key, and a client that pinned it refuses another.

## Sealing credentials with your SSH key

The encrypted StorageClass is the storage layer's, and its key is the cluster's. To keep credentials unreadable from a volume snapshot, a backup or a shared volume even when that layer is not trusted, turn on mu's own sealing: uncomment `[agent] state_key = "ssh"` in the agent's `config.toml` (the `mu-agent-init` ConfigMap has it commented out; edit the real file in the `auth` volume if the agent already started once). The first `mu-remote <remote>` attach shows the public key it would use (`~/.ssh/id_ed25519.pub`, or the remote's `state_key`; an ed25519 key, no RSA) and asks before it enables the sealing. From then on `auth.json` and omp's login database sit on the volume only in sealed form, and omp reads them from a memory-backed directory in the pod (`/dev/shm`). After a pod restart the agent starts locked and waits, setting `waiting` in `mu agent list --json`, until a client with the key attaches and unlocks it.

For an agent that must restart unattended, mu's `state_key_file` or `state_key_command` reads a 32-byte key at start instead; the key's source (a Secret, a KMS) then decides how well it is protected. Details and limits are in [data-flow.md](data-flow.md) and mu's `docs/remote.md`.

## Cluster hygiene

- **Separate namespaces.** Pod security `restricted` is enforced on `mu-agent` and `baseline` on `mu-workers`; the agent, which holds credentials, and the workers, which run model-written code, never share one.
- **Least privilege.** The agent's service account can touch pods in `mu-workers` and nothing else: no secrets, no cluster-wide role. Workers have no service account token.
- **Network policies.** Nothing is let in. Workers and the agent can leave the namespace only to DNS, the public internet and, for the agent, the API server (workers: the object store). Attaching needs no inbound path: it is `kubectl exec` through the API server.
- **Encrypted volumes.** Use an encrypted StorageClass, and treat snapshots and backups of the volumes like `auth.json` itself. That protects against stolen disks, backups and other tenants. It does not protect against whoever administers the cluster, who can reach the storage layer's key; this is why the main agent runs only on a cluster you control. See [data-flow.md](data-flow.md).

# The main agent in a Kubernetes cluster

A generic example, in `scripts/remote/k8s/`, of the setup in [remote.md](remote.md): one main agent that lives in a pod, workers that it starts in a second namespace, and your terminal attached from anywhere. It is for a cluster **you control**. The main agent holds your credentials (`auth.json`), so a cluster whose operator is someone else runs workers at most (an untrusted [remote](remote.md#remotes), which never gets a credential). Every name in the manifests is a placeholder or a neutral default: replace `<image>`, `<storage-class>`, `<object-store-cidr>`, `<object-store-port>` and `<api-server-cidr>`.

## What it creates

| File | Holds |
|---|---|
| `namespaces.yaml` | `mu-agent` (the main agent) and `mu-workers` (the worker pods) |
| `rbac.yaml` | ServiceAccount `mu-agent` and a Role in `mu-workers` only: `create`, `get`, `list`, `watch`, `delete` on pods and `create` on `pods/exec`. No secrets, no ClusterRole |
| `network-policies.yaml` | both namespaces refuse all ingress; `mu-workers` may reach DNS, the internet outside the private ranges and the object store; `mu-agent` may reach DNS, the internet and the API server |
| `storage.yaml` | the PVCs `auth`, `home` and `work` |
| `agent.yaml` | a StatefulSet of one running `mu agent --detach` as uid 1000, and the ConfigMap with its init script and `config.toml` |
| `kustomization.yaml` | all of the above |

The agent container runs as a non-root user, with a read-only root file system, every capability dropped, no privilege escalation and the `RuntimeDefault` seccomp profile; `/tmp` is an `emptyDir`. Its init script starts the agent and follows its log (`kubectl logs`); a liveness probe on `mu agent list --json` restarts the container, and with it the agent, when the agent is gone. Workers keep `automountServiceAccountToken: false` (the pod template of the pod adapter sets it), so a worker has no API access at all.

## Deploy

1. Build two images from a checkout and push them to a registry the cluster can pull from. Use a new tag each time: a node that has a tag does not pull it again.

   ```sh
   git archive --format=tar HEAD | docker build --target worker -t <registry>/mu-worker:<tag> -f scripts/remote/Dockerfile -
   git archive --format=tar HEAD | docker build --target agent  -t <registry>/mu-agent:<tag>  -f scripts/remote/Dockerfile -
   ```

   `worker` is what the pod adapter starts for a task (`<image>` in the agent's config). `agent` is the worker plus omp (for `/login` and MCP sign-ins), `kubectl` and `jq`, `tini` as PID 1, and the pod adapter with its template in `/usr/local/lib/mu/` (`<agent-image>` in `agent.yaml`). `--build-arg KUBECTL_VERSION=…` picks another kubectl, which may be one minor version off the API server's. The images are for x86-64 (the pinned omp is fetched for that platform).
2. Choose a StorageClass that is **encrypted at rest**: the volumes hold repositories with their history, transcripts and `auth.json`. Set it as `<storage-class>` in `storage.yaml`.
3. Fill in the other placeholders. The API server's address is `kubectl get endpoints kubernetes -n default`; the object store is the sccache bucket's endpoint (drop that egress rule if you run no sccache).
4. `kubectl apply -k scripts/remote/k8s/`.

The NetworkPolicies need a CNI that enforces them; without one they are ignored silently.

## The agent's own remote

The `mu-agent-init` ConfigMap carries the `config.toml` the first start copies to the profile home. It names the pod adapter with the pod's in-cluster credentials (no `context`, so `kubectl` uses the service account) and makes it the default remote, so the agent starts its own workers in `mu-workers`:

```toml
default_remote = "workers"
[task]
launch = ["task"]
remote = "workers"

[remote.workers]
adapter = "/usr/local/lib/mu/mu-remote-pod"
trusted = true                  # your cluster; this allows sccache_secret_name
[remote.workers.settings]
namespace = "mu-workers"
image = "<image>"
sccache_secret_name = "mu-sccache"   # optional; a Secret in mu-workers that you create
```

## Attach from your laptop

On the laptop, install the adapter (`mu remote install-adapter pod`) and add a remote that points at the agent's namespace, with your kubeconfig as the credential:

```toml
[remote.cluster]
adapter = "pod"
trusted = true
[remote.cluster.settings]
context = "<kube-context>"
agent_namespace = "mu-agent"
namespace = "mu-workers"
```

`mu -r cluster` lists the agents in `agent_namespace` (the pod carries the labels `app=mu-task`, `mu-kind=agent` that the adapter selects on) and joins the one that runs; `mu -r cluster -c` joins the latest. You need `get`, `list` and `pods/exec` in `mu-agent` through your own kubeconfig. The adapter starts a second, separate agent pod there only when you ask for a new session and no agent can be found; to use only the StatefulSet's agent, attach with `-c` or `--resume`.

## Credentials

Nothing secret goes in a manifest, a Secret or `config.toml`. Run `/login` (or `mu auth add`) inside the attached session: it runs in the pod and writes to the `auth` volume. mu keeps its `auth.json` in its profile home and replaces it by renaming a new file over it (a single-file mount cannot be renamed over), and omp keeps its own credentials in its agent directory, so the `auth` volume is the agent's whole profile home (`auth.json`, sessions, memory, the agent's key) with the omp agent directory (`PI_CODING_AGENT_DIR`, set in `agent.yaml`) inside it, and it is mounted into the agent container only; the `home` volume holds the rest of the user's home (caches, git configuration) and `work` the repository checkouts. The repository first reaches the pod as a bundle from your client (an initial `start --dir`); the pod holds no deploy key.

`mu agent key` in the pod prints the agent's key fingerprint; compare it with what the client shows when it pins the key for the first time (`kubectl exec -n mu-agent mu-agent-0 -- mu agent key`).

## Cluster hygiene

- **Separate namespaces.** The agent, which holds credentials, and the workers, which run model-written code, never share one; pod security `restricted` is enforced on `mu-agent` and `baseline` on `mu-workers`.
- **Least privilege.** The agent's service account can touch pods in `mu-workers` and nothing else: no secrets, no cluster-wide role. Workers have no service account token.
- **Network policies.** Nothing is let in. Workers and the agent can leave the namespace only to DNS, the public internet and, for the agent, the API server (workers: the object store). Attaching needs no inbound path: it is `kubectl exec` through the API server.
- **Encrypted volumes.** Use an encrypted StorageClass, and treat snapshots and backups of the volumes like `auth.json` itself. mu does not encrypt the volumes or hold a key: the key belongs to the storage layer (a cloud provider's own or KMS key, LUKS with a key kept in a Secret or a KMS, or the node's disk encryption). That protects against stolen disks, backups and other tenants. It does not protect against whoever administers the cluster, who can reach the storage layer's key; this is why the main agent runs only on a cluster you control.
- **Stolen disks and other tenants are what this protects against.** It does not protect against the cluster's own administrators or against root on the node: they can read the pod's memory and volumes. For a cluster you do not control, run workers only.

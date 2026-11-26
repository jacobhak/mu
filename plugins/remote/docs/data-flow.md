# Where your data goes

Which kind of remote may hold what:

- **Trusted** (`trusted = true`): a remote you control. It may run the main agent and hold credentials (`auth.json`, the agent's key, deploy keys, object-store keys). Namespaces, least-privilege service accounts, network policies and volumes encrypted at rest protect against other tenants and against stolen disks or backups, not against whoever administers the cluster.
- **Untrusted** (the default): it runs tasks for the repositories in its `repos` only, and never a main agent. Its operator can read the repository with its full history, the task text, the workers' transcripts and the build cache. `mu-remote` sends it no credential: not `auth.json`, no API key or token, no deploy key, no object-store key (the pod adapter refuses its sccache secret there).

Where each kind of data ends up:

| Data | Where | Until |
|---|---|---|
| The repository, with its full history | The worker's pod, in an `emptyDir` (the bundle is also copied in); on mu's side the launcher's bundle (`MU_TASK_BUNDLE`) | The pod and its volume are gone; mu deletes its bundle and ref when the run ends |
| The task text and the worker's transcript | The worker's `mu --headless` session in the pod, and a mirror in your own session store | The pod is deleted; the mirror follows the retention of local sessions (none) |
| Result bundles and refs | `refs/mu/result/<id>` in your repository, and the bundle in the pod until `fetch` succeeds | You delete the ref; unapplied results are kept and listed in mu's Sessions tab |
| Model traffic | Relayed through your mu session: the worker holds no model credential, and may spend only on its task's models up to mu's `[task] max_cost` | The provider's own retention |
| The build cache (sccache bucket) | The object store. Workers on a trusted remote share write access, so one worker can poison another's build | You clear the bucket |
| The main agent's pod (trusted only) | Its volumes: sessions, memory, `auth.json`, the checkouts | You delete the volumes |
| The attach stream | Sealed by [the channel](channel.md) between `mu-remote connect` and `mu-remote bridge`; plain between mu and `connect`, and between `bridge` and the agent | The transport's own retention (ciphertext) |
| Files the agent's machine hands over (`helper read`, trusted only) | A private temporary directory on your machine, made by mu | mu removes it; its retention is mu's |
| Files you upload (`helper write`, trusted only) | `<session local dir>/uploads/` on the remote, mode 0600 | You delete them with the session |
| Forwarded ports (`helper forward`, trusted only) | Bound to `127.0.0.1` on your machine only, through the ssh connection or a `kubectl port-forward` | `unforward`, or the connection or process ends |
| Keys and pins | `<state>/keys/` on each machine: `bridge.key`, `client.key`, `pins.json` | You delete them |

The attach and worker streams are [encrypted](channel.md) on top of ssh or `kubectl exec`, which hides them from the API server and the hops in between, not from root on the node the pod runs on. A cluster you do not control and want to run a main agent on is for a later design (confidential containers).

## What state encryption protects, and against whom

mu's own state encryption (`[agent] state_key = "ssh"`, see mu's `docs/remote.md`) covers the threat at *storage time*: someone who reads the volume without being the running agent (a volume snapshot or backup, a stolen disk, another tenant or a storage administrator reading the volume, a StorageClass that turns out not to be encrypted). With it on, `auth.json` and omp's logins are sealed under a data key that is on the volume only wrapped, for the public keys you chose, so a copy of the volume holds no credential in the clear. It does not cover the operator or root of the node *while the agent runs*: the agent holds the data key in memory, and so can anyone who can read its memory. Sessions, memory, history and artifacts stay readable on the volume (they hold code and transcripts). This is defence in depth for the credentials, not a replacement for an encrypted StorageClass. The `state_unlock` key crosses the attach stream, which is why [the channel](channel.md) ends next to the agent.

## Cluster hygiene

- **Separate namespaces.** The agent, which holds credentials, and the workers, which run model-written code, never share one.
- **Least privilege.** The agent's service account can touch pods in the workers' namespace and nothing else: no secrets, no cluster-wide role. Workers have no service account token.
- **Network policies.** Nothing is let in. Attaching needs no inbound path: it is `kubectl exec` through the API server.
- **Encrypted volumes.** Use an encrypted StorageClass, and treat snapshots and backups of the volumes like `auth.json` itself. Neither mu nor `mu-remote` encrypts the volumes or holds a key: it belongs to the storage layer.
- **Stolen disks and other tenants are what this protects against.** It does not protect against the cluster's own administrators or against root on the node: they can read the pod's memory and volumes. For a cluster you do not control, run workers only, on an untrusted remote.

See [kubernetes.md](kubernetes.md) for the manifests that implement this.

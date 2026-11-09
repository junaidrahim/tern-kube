# tern-kube

`tern-kube` is a fast, native Kubernetes browser for [Tern](https://docs.stencil.so/tern/),
modeled on [Aptakube](https://aptakube.com).
It is a Rust program that draws Tern's native UI through the Tern Surface Protocol.
A small Tern plugin opens it from the command palette.

## How it works

- It reads your kubeconfig with `kube-rs` and talks to the API servers directly.
  Credential plugins such as `tsh` and `gke-gcloud-auth-plugin` run in-process, like kubectl runs them.
- Each list is a list+watch stream (`kube_runtime::watcher`), so tables update live without polling.
  The first list is served from the apiserver's watch cache.
- Recent watches stay open, so switching back to a kind is instant.
- Watch events are batched into one frame. A large table waits longer between frames, up to 500 ms.
- Each context gets its own client, created in the background.
  A slow sign-in in one cluster does not hold up another.
- It never changes your kubeconfig, and it runs no agent in the cluster.

## Functions

- **Clusters**
  - Pick one context, or merge several into one table with a Context column (**C**, or the layers button).
  - Namespace filter.
- **Resources**
  - Workloads: Pods, Deployments, StatefulSets, DaemonSets, ReplicaSets, Jobs, CronJobs, HPAs.
  - Network: Services, Ingresses, Endpoint Slices, Network Policies.
  - Config: ConfigMaps, Secrets.
  - Storage: PVCs, PVs, Storage Classes.
  - Cluster: Nodes, Namespaces, Events.
  - Access control: Service Accounts, Roles, Role Bindings, Cluster Roles, Cluster Role Bindings.
  - Every custom resource that discovery finds.
- **Tables**
  - Live tables with sortable columns and kubectl-style status (for example `CrashLoopBackOff` or `Init:0/1`).
  - CPU and memory for pods and nodes from the Metrics API (metrics-server).
    Bars measure usage against the limit, else the request.
- **Filter**
  - Words must all match.
  - `a|b` matches either.
  - `!x` excludes.
  - `*` is a wildcard.
  - `/regex/` is a regular expression.
- **Detail pane**
  - **Overview:** metadata, owner links, kind details, containers, child pods of workloads, services and nodes, conditions, labels and annotations.
  - **YAML.**
  - **Events:** live.
  - **Logs:**
    - Streamed, with a container picker and the previous instance's log.
    - Workloads and services merge the logs of all their pods, each line tagged with its pod.
- **Actions** (row right-click menu, detail buttons or keys)
  - Scale, rollout restart and delete.
  - CronJob trigger, suspend and resume.
  - Node cordon and uncordon.
  - Each one asks first and names the context.
  - Shell, Edit YAML (`kubectl edit`) and Follow log (`kubectl logs -f`) open in a real terminal split beside the browser.
- **Port forwards**
  - Pods and services, listed in the sidebar with live status.
  - A service forward picks a ready pod for each new connection, so it follows pod replacements.
- **Navigation**
  - Back and forward history.
  - `g` jumps to a kind.

## Install

```sh
cargo install --path .        # puts tern-kube in ~/.cargo/bin
tern plugin link plugin       # the palette command and key bind
```

Open the command palette and choose **Open Kubernetes**, or press `⌥⇧⌘K`.
You can also run `tern-kube` in any Tern pane:

```sh
tern-kube [--context NAME]... [--namespace NAME]
```

Give `--context` more than once to merge clusters.
Shell, Edit and Follow log call `kubectl`, so `kubectl` must be on the pane's `PATH`.
Everything else talks to the API servers directly.

## Change protection

Contexts can be protected or read-only, as in Aptakube.

- **Protected:** a change or a shell first asks to unlock the context for 15 minutes.
- **Read-only:** changes and shells are refused.

Without a config file, every context with `prod` in its name is protected.
Edit `~/.config/tern-kube/config.json` to change that:

```json
{ "protected": ["*prod*"], "readonly": ["audit-*"], "unprotected": ["prod-sandbox"] }
```

Patterns are globs over context names. Matching ignores letter case.
`unprotected` beats `readonly`, which beats `protected`.
RBAC still decides what your account may do.

## Keys

| Key | Function |
| --- | --- |
| ↑ ↓ / j k, PgUp PgDn, Home End | Select a row |
| Enter | Open the detail pane |
| Esc | Close a menu, dialog or the detail pane, or clear the filter |
| / | Filter |
| [ ] | Previous or next kind |
| g then p d t a r j o s i c x v n e | Pods, Deployments, StatefulSets, DaemonSets, ReplicaSets, Jobs, CronJobs, Services, Ingresses, ConfigMaps, Secrets, PVCs, Nodes, Events |
| ⌥← ⌥→ | Back, forward |
| Tab, Shift+Tab, 1–4 | Detail tabs (y is YAML) |
| l | Logs |
| s | Shell into the pod |
| f | Port forward (pods, services) |
| Shift+S | Scale |
| Shift+R | Rollout restart |
| t | Trigger the CronJob |
| e | `kubectl edit` |
| Delete, ⌘⌫ | Delete |
| Shift+C | Pick contexts |
| q, Ctrl+C | Quit |

The last contexts, kind and namespace are saved in `~/.local/state/tern-kube/state.json`.

## Development

```sh
cargo test
cargo build --release && cp target/release/tern-kube ~/.cargo/bin/
```

`PLAN.md` has the design.

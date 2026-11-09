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
- The eight most recent watches stay open, so switching back to a kind is instant.
- Watch events are batched into one frame. A large table waits longer between frames, up to 500 ms.
- It never changes your kubeconfig, and it runs no agent in the cluster.

## Functions

- Context and namespace selection.
- A sidebar of resource kinds:
  - Workloads: Pods, Deployments, StatefulSets, DaemonSets, ReplicaSets, Jobs, CronJobs, HPAs.
  - Network: Services, Ingresses, Endpoint Slices, Network Policies.
  - Config: ConfigMaps, Secrets.
  - Storage: PVCs, PVs, Storage Classes.
  - Cluster: Nodes, Namespaces, Events.
  - Access control: Service Accounts, Roles, Role Bindings, Cluster Roles, Cluster Role Bindings.
  - Every custom resource that discovery finds.
- Live tables with sortable columns and kubectl-style status (for example `CrashLoopBackOff` or `Init:0/1`).
- A filter:
  - Words must all match.
  - `a|b` matches either.
  - `!x` excludes.
  - `*` is a wildcard.
  - `/regex/` is a regular expression.
- A detail pane with these tabs:
  - **Overview:** metadata, kind details, containers, conditions, labels and annotations.
  - **YAML.**
  - **Events:** live.
  - **Logs:** streamed, with a container picker and the previous instance's log.
- Shell, `kubectl edit` and `kubectl logs -f` open in a real terminal split beside the browser.

## Install

```sh
cargo install --path .        # puts tern-kube in ~/.cargo/bin
tern plugin link plugin       # the palette command and key bind
```

Open the command palette and choose **Open Kubernetes**, or press `⌥⇧⌘K`.
You can also run `tern-kube` in any Tern pane:

```sh
tern-kube [--context NAME] [--namespace NAME]
```

Shell, Edit and Follow in terminal call `kubectl`, so `kubectl` must be on the pane's `PATH`.
The browser itself does not need `kubectl`.

## Keys

| Key | Function |
| --- | --- |
| ↑ ↓ / j k, PgUp PgDn, Home End | Select a row |
| Enter | Open the detail pane |
| Esc | Close a menu, the detail pane, or clear the filter |
| / | Filter |
| [ ] | Previous or next kind |
| Tab, Shift+Tab, 1–4 | Detail tabs (y is YAML) |
| l | Logs of the selected pod |
| s | Shell into the selected pod |
| e | `kubectl edit` the selected object |
| q, Ctrl+C | Quit |

The last context, kind and namespace are saved in `~/.local/state/tern-kube/state.json`.

## Development

```sh
cargo test
cargo build --release && cp target/release/tern-kube ~/.cargo/bin/
```

`PLAN.md` has the design and the milestones.

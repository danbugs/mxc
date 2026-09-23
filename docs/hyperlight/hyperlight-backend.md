# Hyperlight Backend

Runs Python inside a [Hyperlight](https://github.com/hyperlight-dev/hyperlight)
micro-VM booting a [Unikraft](https://unikraft.org/) unikernel, driven
in-process by the [`hyperlight-unikraft`](https://github.com/hyperlight-dev/hyperlight-unikraft)
crate. One code path serves Linux (KVM) and Windows (WHP).

## At a glance

| | |
|---|---|
| **Binary** | `lxc-exec` (Linux), `wxc-exec.exe` (Windows) |
| **Config value** | `"containment": "hyperlight"` |
| **Schema** | `0.9.0-alpha`, with `--experimental` |
| **Requires** | x86_64; `/dev/kvm` readable and writable, or WHP enabled; a build with `--with-hyperlight` |
| **Isolation** | Hardware virtualization; the guest is a unikernel with its own filesystem |
| **Guest** | The `agent` rootfs: CPython 3.12 with numpy, pandas, scipy and scikit-learn already imported |
| **Cold start** | A snapshot restore |

## Quick start

```sh
./build.sh --with-hyperlight        # Linux; build.bat --with-hyperlight on Windows
lxc-exec --setup-hyperlight         # once per machine: pulls the rootfs, warms the snapshot
```

```json
{
    "version": "0.9.0-alpha",
    "process": {
        "commandLine": "import pandas as pd\nprint(pd.DataFrame({'x': [1, 2]}).sum().to_dict())",
        "timeout": 30000
    },
    "containment": "hyperlight"
}
```

```sh
lxc-exec --experimental pandas.json
```

`commandLine` is Python source, passed to the interpreter as is. The
guest's `print` output goes straight to the process's stdout.

## The image home

Setup fills an image home with three things:

| Entry | Purpose |
|---|---|
| `initrd.cpio` | The guest rootfs, pulled from `ghcr.io/hyperlight-dev/hyperlight-unikraft/agent` at the release the crate is pinned to |
| `snapshot/` | The guest, booted once and captured with the interpreter warm; every run restores it |
| `VERSION` | The rootfs release the home holds |

The kernel is embedded in the crate, so nothing else is downloaded. The
pull talks to the registry directly; no container runtime is involved.

The runner looks for a home in this order and takes the first that holds a
snapshot this build loads, or a rootfs of this release to warm one from:

1. `$MXC_HYPERLIGHT_HOME`
2. `~/.local/share/mxc-hyperlight/` on Linux, `%LOCALAPPDATA%\mxc-hyperlight\` on Windows
3. `<exe dir>/mxc-hyperlight/`
4. `<cwd>/.mxc-hyperlight/`

Setup writes to the first or second of these. The rootfs is needed only
to warm, so a home trimmed to just the snapshot still runs.

A snapshot loads only under the build that saved it (the crate keys it by
its kernel and host contract), and a rootfs only boots on its own
release's kernel. After a crate upgrade, `--setup-hyperlight` rebuilds a
home from another release, and a run warms a fresh snapshot by itself
when only the snapshot is stale. `--force` rewarms a home that is already
current, for example after dropping in a rootfs of your own.

## How a run works

Every request restores the warm snapshot with its own mounts and network
policy, so consecutive runs are hermetic. A runner reused within one
process rewinds to the snapshot between calls.

| Aspect | Behaviour |
|---|---|
| Exit code | The script's: `sys.exit(N)` gives `N`, an uncaught exception gives `1`, a runner error gives `-1` with the reason in `error_message` |
| Timeout | `process.timeout` bounds the run; a guest that overruns is interrupted and the run reports `execution timed out` |
| stdout / stderr | Inherited by the guest; `ScriptResponse.standard_out` stays empty, so capture at the process level |

## Configuration

| Field | Behaviour |
|---|---|
| `process.commandLine` | Python source |
| `process.timeout` | Run timeout in milliseconds |
| `filesystem.readwritePaths` | Each directory appears in the guest at `/host/<basename>` |
| `filesystem.readonlyPaths` | The same, mounted read-only; the guest kernel and the host both enforce it |
| `filesystem.deniedPaths` | Checked at preflight against the two lists above |
| `network.defaultPolicy` | `allow` gives the guest host-proxied sockets; `block` refuses every `socket()` |
| `network.allowedHosts` / `blockedHosts` | An allow list or a block list of hosts; one or the other |
| `network.proxy` | Rejected at preflight |
| `workingDirectory` | Rejected at preflight; the guest has its own filesystem |

Mount directories are created when their parent exists. A basename may not
contain whitespace, `:` or brackets, and two mounts may not share one.

## What the guest has

Present in the rootfs: numpy, pandas, scipy, scikit-learn, matplotlib,
Pillow, pydantic, PyYAML, Jinja2, BeautifulSoup, tabulate, tqdm, openpyxl,
pypdf, lxml, cryptography, dateutil, requests, httpx and pip. numpy,
pandas, scipy, scikit-learn, matplotlib, Pillow and dateutil are imported
before the snapshot is taken, so importing them costs nothing at run
time. `subprocess` works, and with network allowed, `pip install` fetches
packages from PyPI into the guest's writable scratch.

## Other guest runtimes

hyperlight-unikraft publishes rootfs images for Node.js, .NET, Go, Rust,
C, Bash and PowerShell beside Python, each with the same boot, snapshot
and mount machinery. The backend's runner is built around one rootfs and
one snapshot per home, so another runtime is a second home and a
selector in the request.

## Design notes

- **In process.** Hyperlight is a Rust library and the executors are Rust
  binaries, so the backend links it and boots the VM in the executor's own
  process: no helper process, pipes or lifecycle to manage.
- **A containment value of its own.** The backend needs a warmed snapshot
  in place before the first run, executes source rather than a command
  line, and forwards host directories to the guest live, so it is
  selected explicitly rather than resolved from `vm` or `microvm`.
- **Images come from upstream.** The rootfs is built and published by
  hyperlight-unikraft's pipeline, and the kernel ships inside the crate.
  Adding a package to the guest is a change to the image there.
- **Experimental.** The backend sits behind `--experimental` while the
  request surface settles; the runtime selector above is the next step.

## Troubleshooting

| Message | Meaning |
|---|---|
| `no hyperlight image found` | No home holds an image; run `--setup-hyperlight`, or set `MXC_HYPERLIGHT_HOME` |
| `holds a rootfs from another hyperlight-unikraft release` | The home predates the crate the binary was built with; `--setup-hyperlight` rebuilds it |
| `hyperlight backend unavailable` | No hypervisor: KVM is missing or not accessible, or WHP is not enabled |
| `execution timed out after N s` | The script overran `process.timeout` |

Run with `--debug` to see the home chosen, restore and call timings, and
the reason behind any runner error.

# ttop

What's eating my machine? `ttop` shows memory (RAM + swap) and CPU grouped by
tmux session, and within each session by process type.

```
$ ttop      # excerpt
                                    procs     total       RAM      swap       CPU  █ RAM  ▓ swap

[containers]                          885    9.5 GB    9.3 GB    194 MB     68.2%  █████
  k3s · 33 pods                       197    2.6 GB    2.6 GB         –     18.0%  █▍
  k3d gbandit-main · 32 pods          224    2.3 GB    2.2 GB    124 MB     16.0%  █▎
  k3d gbandit-gba-157-158 · 28 pods   196    2.3 GB    2.3 GB      9 MB     17.3%  █▎
  k3d gbandit-gba-179 · 32 pods       256    2.1 GB    2.1 GB     10 MB     16.9%  █▏
  buildx_buildkit_gbandit-builder0      2    141 MB    124 MB     17 MB         –  ▏
  … 5 more                             10     87 MB     53 MB     35 MB      0.0%

_corc-sessions                         74   11.5 GB    9.3 GB    2.2 GB     17.0%  █████▓
  claude                               31    7.2 GB    6.2 GB    1.0 GB     16.0%  ███▊
  rust-analyzer                         1    2.9 GB    2.9 GB         –         –  █▌
  node                                 11    779 MB     27 MB    751 MB         –  ▍
  npm                                  11    464 MB     24 MB    440 MB         –  ▎
  … 9 more types                       20    212 MB    206 MB      5 MB      1.0%  ▏
───────────────────────────────────────────────────────────────────────────────────────────────────────
total                                1244   38.3 GB   32.0 GB    6.3 GB    233.7%  █████████████████▓▓▓

                                               used     total      free         %
RAM                                         33.9 GB   47.0 GB   13.1 GB       72%  ██████████████░░░░░░  cache 18.1 GB · processes 29.8 GB
swap zram0                                   6.0 GB   15.7 GB    9.7 GB       38%  ████████░░░░░░░░░░░░  zram, uses 2.2 GB RAM (2.7× compressed)
swap swapfile                                4.6 GB   32.0 GB   27.4 GB       14%  ███░░░░░░░░░░░░░░░░░  disk
CPU                                          231.5%   1200.0%    968.5%       19%  ████░░░░░░░░░░░░░░░░  processes 233.7% · 12 cores · load 4.00 7.05 8.48 · sampled 1.0 s
```

- **Per tmux session**: every process is attributed to the session it was
  started from, including orphaned daemons.
- **Containers and VMs as one row each**: Docker, LXC, Kubernetes clusters
  (k3s, k3d) and qemu VMs outside tmux get their own groups.
- **Honest numbers**: memory is PSS + SwapPss, so shared memory is split
  proportionally and the totals add up. zram and zswap are shown separately.
- **Only what matters**: small rows and sessions are folded into "… N more";
  `--all` shows everything.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/HectorBjernersjo/ttop/releases/latest/download/ttop-installer.sh | sh
```

The binary goes into `~/.local/bin`. Prebuilt binaries are also on
[Releases](https://github.com/HectorBjernersjo/ttop/releases), or build it yourself:

```sh
cargo install --git https://github.com/HectorBjernersjo/ttop
```

ttop reads `/proc`, so it only works on Linux.

## Usage

```sh
ttop          # sort by memory (default)
ttop --cpu    # sort by CPU
ttop --all    # show every row and session, not just the big ones
```

See [how it works](docs/how-it-works.md) for how processes are grouped and measured.

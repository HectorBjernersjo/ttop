# How ttop works

- **Session:** a process belongs to the tmux session whose pane is in its
  parent chain. Orphaned daemons are caught through `TMUX_PANE` in the
  process environment.
- **Containers:** Docker, LXC and Kubernetes outside tmux are collected under
  `[containers]`, one row per container or cluster.
- **VMs:** qemu machines (Incus, LXD, libvirt or standalone) outside tmux are
  collected under `[VMs]`, one row per VM, named from `-name`. Inside a tmux
  session they show up as `qemu <name>` instead of a shared row.
- Everything else ends up in `[outside tmux]`.
- **Memory:** PSS and SwapPss from `/proc/<pid>/smaps_rollup`, so shared memory
  is counted proportionally and the totals add up. zram and zswap get their
  own rows.
- **CPU:** measured over one second and shown as a percentage of one core
  (400% = four full cores).
- **Pruning:** rows under 1% of their session and sessions under 2% of the
  total are folded into "… N more". `--all` turns this off.

At the bottom there is a summary of RAM, swap and CPU for the whole machine.

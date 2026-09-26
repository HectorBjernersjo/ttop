# ttop

Vad äter min maskin? `ttop` visar minne (RAM + swap) och CPU grupperat per
tmux-session, och inom varje session per processtyp.

## Installera

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/HectorBjernersjo/ttop/releases/latest/download/ttop-installer.sh | sh
```

Binären hamnar i `~/.local/bin`. Färdiga binärer finns även under
[Releases](https://github.com/HectorBjernersjo/ttop/releases), eller bygg själv:

```sh
cargo install --git https://github.com/HectorBjernersjo/ttop
```

ttop läser `/proc` och fungerar därför bara på Linux.

## Användning

```sh
ttop          # sortera på minne
ttop --cpu    # sortera på CPU
ttop --all    # visa alla rader och sessioner, inte bara de stora
```

```
                                   antal     summa       RAM      swap       CPU  █ RAM  ▓ swap
_corc                                 13    2.7 GB    459 MB    2.2 GB      7.0%  ▓▌
  rust-analyzer                        1    2.3 GB     49 MB    2.2 GB         –  ▓▎
  claude                               1    362 MB    338 MB     23 MB      3.0%  ▎

[containrar]                        1038   11.9 GB   11.4 GB    453 MB    109.9%  ██████▌
  k3s · 33 pods                      197    2.7 GB    2.7 GB         –     13.3%  █▌
  k3d gbandit-main · 32 pods         224    2.3 GB    2.2 GB    146 MB     19.8%  █▎
```

## Hur det funkar

- **Session:** en process hör till den tmux-session vars pane finns i dess
  förälderkedja. Föräldralösa daemons fångas via `TMUX_PANE` i processens
  environment.
- **Containrar:** Docker, LXC och Kubernetes utanför tmux samlas under
  `[containrar]`, en rad per container eller kluster.
- **VM:ar:** qemu-maskiner (Incus, LXD, libvirt eller fristående) utanför
  tmux samlas under `[VM:ar]`, en rad per VM med namnet ur `-name`. Inuti
  en tmux-session blir de `qemu <namn>` istället för en gemensam rad.
- Resten hamnar i `[utanför tmux]`.
- **Minne:** PSS och SwapPss ur `/proc/<pid>/smaps_rollup`, så delat minne
  räknas proportionellt och summorna går ihop. zram och zswap visas som egna
  rader.
- **CPU:** mäts under en sekund och visas i procent av en kärna (400 % = fyra
  fulla kärnor).
- **Beskärning:** rader under 1 % av sessionen och sessioner under 2 % av
  totalen slås ihop till "… N till". `--all` stänger av det.

Längst ner visas en sammanfattning av RAM, swap och CPU för hela maskinen.

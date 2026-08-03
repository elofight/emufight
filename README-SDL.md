# emufight-sdl

Reference **SDL2** host for the `emufight` library (same git workspace).

| Mode | Command |
|---|---|
| Offline | `cargo run --bin emufight-sdl --features sdl-host --release -- kof98` |
| Host (P0) | `… --listen 127.0.0.1:7000` |
| Join (P1) | `… --connect 127.0.0.1:7000` |

Join only needs the host address. Host latches the joiner from the first UDP packet, then both use those endpoints for GGRS.

## Prerequisites

- Repo built from root (`vendor/ymfm` submodule, C++)
- SDL2 (`brew install sdl2` / `libsdl2-dev`)
- Licensed ROMs under `roms/<name>/` (never shipped)
- Boot states under `boot/<name>/charselect.bin` (shipped)

## Capture boot state

```sh
cargo run --bin emufight-capture-boot -- kof98
```

## Keys (Elofight product defaults)

| | |
|---|---|
| **WASD** | Stick (arrows also work) |
| **I J O K** | A B C D — NeoGeo / kof98 |
| **I O P · J K L** | A B C · D E F — 6-button (sf2…) |
| **1 / 3 / 5** | Start / Select / Coin |
| Esc | Quit |

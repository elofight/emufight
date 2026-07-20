# Netplay boot savestates

**ROM sets are never part of this repository.**  
This tree holds only **character-select / match-ready** savestates so online peers share the same frame-0.

```text
boot/<game_id>/charselect.bin
```

| Game   | File                         | Notes                          |
|--------|------------------------------|--------------------------------|
| kof98  | `kof98/charselect.bin`       | 2P versus character select     |
| sf2ce  | `sf2ce/charselect.bin`       | 2P player select (CPS-1)       |

## Capture (with your own dumps)

Place licensed ROMs under `roms/<game>/` (and NeoGeo system ROMs under `data/neogeo/` or `roms/neogeo/`), then:

```sh
cargo run -p emufight-sdl --bin emufight-capture-boot -- kof98
cargo run -p emufight-sdl --bin emufight-capture-boot -- sf2ce
```

Output defaults to `boot/<game>/charselect.bin`.

## Adding a game

1. Emulation support (cart / CPS config) in the library.  
2. Capture recipe in `emufight-capture-boot` (or document a manual sequence).  
3. Commit **only** `boot/<id>/charselect.bin` (+ optional `.ppm` screenshot).  
4. Both netplay peers must use the **same** bin and core revision.

Load order at runtime: `boot/` → `data/` → `roms/` (see `emufight::boot`).

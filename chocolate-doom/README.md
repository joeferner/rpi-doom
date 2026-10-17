# From Chocolate Doom

OPL music playback, copied from [Chocolate Doom](https://github.com/chocolate-doom/chocolate-doom)
release `chocolate-doom-3.1.0` (commit `35fb1372d10756ca27eca05665bd8a7cebc71c05`),
GPL v2 (`COPYING.md`). doomgeneric is derived from Chocolate Doom but
leaves this part out.

| File | Upstream | What it does |
|---|---|---|
| `i_oplmusic.c` | `src/i_oplmusic.c` | The music module: plays MIDI on an OPL using the WAD's `GENMIDI` instruments, the way DOS Doom's DMX library did |
| `midifile.c`, `midifile.h` | `src/` | Reads the MIDI that doomgeneric's `mus2mid.c` makes from a `D_*` lump |
| `opl.h` | `opl/opl.h` | The `OPL_*` interface the music module talks to |
| `opl_queue.c`, `opl_queue.h` | `opl/` | The time-ordered queue of callbacks the music module schedules |

Not copied: `opl/opl.c` and the drivers. This board's `OPL_*`
implementation is `c/opl_rpi.c`, and the chip is Nuked OPL3 in Rust
(the nuked-opl3 crate), rendered on core 1.

Every change from upstream is marked `rpi-doom:` in the source:

- `rpi_compat.h` (new): what both files need from Chocolate Doom's
  headers that doomgeneric's versions lack: `PACKED_STRUCT`
  (`doomtype.h`), `SDL_SwapBE16`/`32` (`i_swap.h` gets them from SDL),
  and `I_Realloc` (`i_system.c`). Included by both.
- `i_oplmusic.c`: the `opl_driver_ver_t` enum and `I_SetOPLDriverVer`
  declaration it needs are declared locally; doomgeneric's `i_sound.h`
  doesn't have them.
- `i_oplmusic.c`: neither the device list nor `music_opl_module` is
  `const`, to match doomgeneric's `music_module_t` and its declaration of
  `music_opl_module` in `i_sound.h`.

- `midifile.c`, `i_oplmusic.c`: songs are converted and parsed in memory
  (`MIDI_LoadMemory`, over newlib's `fmemopen`) instead of written to a
  temporary file and read back with `MIDI_LoadFile`. On this board that
  would be a card write and delete per song change, into a temporary
  directory the card doesn't have. The parser itself is unchanged.

Its callbacks run on core 1, which must not use newlib's stdio or heap.
They don't: the only `printf`s on those paths are compiled out
(`OPL_MIDI_DEBUG`), and songs are parsed and allocated in
`RegisterSong`/`PlaySong`, on core 0.

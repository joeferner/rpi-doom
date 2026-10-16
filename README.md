# rpi-doom

Doom on a bare-metal Raspberry Pi 2 (B): the
[doomgeneric](https://github.com/ozkl/doomgeneric) engine on
[rpi-hal](https://crates.io/crates/rpi-hal) and
[rpi-kickstart](https://crates.io/crates/rpi-kickstart), with no
operating system. The engine is C, built against newlib; the layer under
it (memory, files on the SD card, the display, the keyboard) is Rust.

## What you need

- A display on HDMI or the DSI connector, and a USB keyboard. Plug both
  in before powering on.
- A USB-to-serial cable (3.3 V): its RX on GPIO14 (pin 8), its TX on
  GPIO15 (pin 10), and ground on pin 6. The console runs at 115200 8N1.
- An SD card with one FAT32 partition.
- `arm-none-eabi-gcc` and newlib (on Arch, `arm-none-eabi-gcc` and
  `arm-none-eabi-newlib`), which compile the engine and supply its C
  library.
- Rust via [rustup](https://rustup.rs). `rust-toolchain.toml` installs
  the right toolchain and target on first build.
- `cargo install cargo-binutils rpi-loader`: `cargo objcopy` turns the
  build into the raw image the firmware loads, and `rpi-loader` uploads
  it to the board.

## Set up the card, once

The game data isn't in this repository: `DOOM.WAD` is commercial. Put a
WAD (`DOOM.WAD`, `DOOM2.WAD`, or the shareware `DOOM1.WAD`) in a `doom/`
directory here first; it's ignored by git.

```sh
make sd-card
```

This fetches the Raspberry Pi boot firmware and the
[rpi-loader](https://github.com/joeferner/rpi-hal/tree/main/loader) image
into `sd-card/`, checking each against `sd-card.sha256`, and copies the
WADs into `sd-card/doom/`. Copy everything in `sd-card/` onto the card's
boot partition, including `config.txt` and the `doom` directory.

The loader goes on the card as `kernel7.img`, the only filename the
firmware loads on this board. It stays resident, so after this you never
have to take the card out again.

To add or replace WADs later without taking the card out, reset the board
so the loader is running and upload them over the serial cable:

```sh
make wads DEVICE=/dev/ttyUSB0
```

## Build and run

```sh
make run DEVICE=/dev/ttyUSB0
```

This builds `target/kernel7.img`, uploads it to the board over the serial
cable, and boots it. Doom's startup log appears on the same cable, and
the game on the display.

To boot without the loader, copy `target/kernel7.img` onto the card in
place of the loader's `kernel7.img`.

## Controls

| Key | Action |
|---|---|
| Arrow keys | Move forward/back, turn |
| Ctrl | Fire |
| Space | Use (open doors, flip switches) |
| Shift (hold) | Run |
| Alt (hold) | Strafe with the left/right arrows |
| `,` / `.` | Strafe left / right |
| 1-7 | Select weapon |
| Tab | Automap |
| Esc | Menu |
| Enter | Select in menus |
| `-` / `=` | Shrink / grow the view (zoom on the automap) |
| F1-F12 | The original's function keys: help, save, load, volume, ... |
| Pause | Pause |

Letters are passed through as typed, so the menus' y/n prompts and the
cheat codes work.

There's no key-binding menu, as in the original. To rebind, edit the
`key_*` lines in `doom/default.cfg`; the values are DOS keyboard
scancodes (e.g. `key_fire 29` is Ctrl, `key_use 57` is Space, `key_up 17`
is W). A `DEFAULT.CFG` from DOS Doom works as-is.

Settings are saved to `doom/default.cfg` on the card when you quit from
the menu (Esc, Quit Game), as in the original; turning the board off
without quitting loses changes made since the last quit. Saving games
doesn't work yet.

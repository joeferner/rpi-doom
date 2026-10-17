//! Compiles the doomgeneric engine (doomgeneric/doomgeneric, a squashed
//! git subtree of github.com/ozkl/doomgeneric) into a static library.
//!
//! The C is built with `arm-none-eabi-gcc` against newlib, which supplies
//! the libc the engine expects (`stdio`, `malloc`, `string`, `math`).
//! Newlib's own OS layer (`_sbrk`, `_read`, `_write`, ...) is left for
//! the Rust side to provide (src/newlib.rs).

use std::path::{Path, PathBuf};
use std::process::Command;

const ENGINE_DIR: &str = "doomgeneric/doomgeneric";

/// This board's C, compiled with the engine: its sound and music modules
/// and its OPL library, which need the engine's headers and WAD functions.
const OUR_C_DIR: &str = "c";
const OUR_SOURCES: &[&str] = &["i_rpisound.c", "opl_rpi.c"];

/// OPL music playback from Chocolate Doom, which doomgeneric leaves out
/// (see its README).
const CHOCOLATE_DIR: &str = "chocolate-doom";
const CHOCOLATE_SOURCES: &[&str] = &["i_oplmusic.c", "midifile.c", "opl_queue.c"];

/// The engine's sources: upstream's Makefile list without its X11
/// platform file (`doomgeneric_xlib.c`). The platform layer for this
/// board, the `DG_*` functions, is written in Rust.
const SOURCES: &[&str] = &[
    "dummy.c",
    "am_map.c",
    "doomdef.c",
    "doomstat.c",
    "dstrings.c",
    "d_event.c",
    "d_items.c",
    "d_iwad.c",
    "d_loop.c",
    "d_main.c",
    "d_mode.c",
    "d_net.c",
    "f_finale.c",
    "f_wipe.c",
    "g_game.c",
    "hu_lib.c",
    "hu_stuff.c",
    "info.c",
    "i_cdmus.c",
    "i_endoom.c",
    "i_joystick.c",
    "i_scale.c",
    "i_sound.c",
    "i_system.c",
    "i_timer.c",
    "memio.c",
    // MUS to MIDI, for the OPL music module.
    "mus2mid.c",
    "m_argv.c",
    "m_bbox.c",
    "m_cheat.c",
    "m_config.c",
    "m_controls.c",
    "m_fixed.c",
    "m_menu.c",
    "m_misc.c",
    "m_random.c",
    "p_ceilng.c",
    "p_doors.c",
    "p_enemy.c",
    "p_floor.c",
    "p_inter.c",
    "p_lights.c",
    "p_map.c",
    "p_maputl.c",
    "p_mobj.c",
    "p_plats.c",
    "p_pspr.c",
    "p_saveg.c",
    "p_setup.c",
    "p_sight.c",
    "p_spec.c",
    "p_switch.c",
    "p_telept.c",
    "p_tick.c",
    "p_user.c",
    "r_bsp.c",
    "r_data.c",
    "r_draw.c",
    "r_main.c",
    "r_plane.c",
    "r_segs.c",
    "r_sky.c",
    "r_things.c",
    "sha1.c",
    "sounds.c",
    "statdump.c",
    "st_lib.c",
    "st_stuff.c",
    "s_sound.c",
    "tables.c",
    "v_video.c",
    "wi_stuff.c",
    "w_checksum.c",
    "w_file.c",
    "w_main.c",
    "w_wad.c",
    "z_zone.c",
    "w_file_stdc.c",
    "i_input.c",
    "i_video.c",
    "doomgeneric.c",
];

const COMPILER: &str = "arm-none-eabi-gcc";

/// The Rust target's code model: 32-bit ARM state, no FPU (soft float
/// ABI), and no unaligned accesses, since `armv7a-none-eabi` is a
/// strict-alignment target. Also what picks newlib's matching build.
const ARCH_FLAGS: &[&str] = &[
    "-marm",
    "-march=armv7-a",
    "-mfloat-abi=soft",
    "-mno-unaligned-access",
];

fn main() {
    let dir = Path::new(ENGINE_DIR);
    let mut build = cc::Build::new();
    build.compiler(COMPILER);
    for flag in ARCH_FLAGS {
        build.flag(flag);
    }
    build
        .files(SOURCES.iter().map(|source| dir.join(source)))
        .files(
            OUR_SOURCES
                .iter()
                .map(|source| Path::new(OUR_C_DIR).join(source)),
        )
        .files(
            CHOCOLATE_SOURCES
                .iter()
                .map(|source| Path::new(CHOCOLATE_DIR).join(source)),
        )
        .include(dir)
        .include(CHOCOLATE_DIR)
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
        .define("NORMALUNIX", None)
        .define("_DEFAULT_SOURCE", None)
        // Sound and music modules: ours, in c/i_rpisound.c.
        .define("FEATURE_SOUND", None)
        // 1997 code: these are noise, not anything to act on here.
        .flag("-Wno-unused-parameter")
        .flag("-Wno-sign-compare")
        .flag("-Wno-missing-field-initializers")
        .flag("-Wno-implicit-fallthrough")
        .flag("-Wno-unused-but-set-variable");
    build.compile("doomgeneric");

    // newlib: libc for the C library itself, libm for `<math.h>`. The
    // compiler ships one build of each per architecture and float ABI,
    // and asking it with the same flags finds the one these objects
    // were compiled for.
    println!("cargo:rustc-link-search=native={}", newlib_dir().display());
    println!("cargo:rustc-link-lib=static=c");
    println!("cargo:rustc-link-lib=static=m");

    println!("cargo:rerun-if-changed={ENGINE_DIR}");
    println!("cargo:rerun-if-changed={OUR_C_DIR}");
    println!("cargo:rerun-if-changed={CHOCOLATE_DIR}");
}

/// The directory holding the `libc.a` that matches [`ARCH_FLAGS`].
fn newlib_dir() -> PathBuf {
    let output = Command::new(COMPILER)
        .args(ARCH_FLAGS)
        .arg("-print-file-name=libc.a")
        .output()
        .unwrap_or_else(|e| panic!("running {COMPILER}: {e}"));
    let libc = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    // Given a name it can't find, the compiler echoes the name back.
    assert!(
        libc.is_absolute(),
        "{COMPILER} has no newlib libc.a; install arm-none-eabi-newlib"
    );
    libc.parent().unwrap().to_path_buf()
}

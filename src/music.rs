//! The OPL chip the music plays on, and the lock around it.
//!
//! Chocolate Doom's music module (chocolate-doom/i_oplmusic.c) turns songs
//! into OPL register writes, through this board's `OPL_*` library
//! (c/opl_rpi.c). That library keeps its state in C and calls down to here
//! for the chip itself, Nuked OPL3 (src/opl3.rs), and for its lock.
//!
//! # The lock
//!
//! Core 0 changes songs and volume; core 1 renders the chip and fires the
//! music module's callbacks. Both touch the chip, the callback queue and
//! the music module's voices, so one lock covers all of it. It's
//! re-entrant: the music module locks inside calls that c/i_rpisound.c
//! has already locked.
//!
//! It's its own lock rather than `critical-section`, which core 0 holds
//! through whole SD card reads (src/files.rs): core 1 waiting on that
//! could run the output dry. Core 0 holds this one only for a song change
//! or a volume change, and core 1 for one chunk's render, about 3 ms.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use alloc::boxed::Box;
use rpi_hal::cpu::core_id;

use crate::opl3::{self, Opl3Chip};
use crate::sound;

/// The core holding the lock, or [`NO_OWNER`].
static OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

/// How deep the owning core is. Only the owner touches it.
static DEPTH: AtomicU32 = AtomicU32::new(0);

const NO_OWNER: u32 = u32::MAX;

/// The chip, made by the first [`rpi_doom_opl_reset`] on core 0. Only
/// reached under the lock.
static CHIP: AtomicPtr<Opl3Chip> = AtomicPtr::new(ptr::null_mut());

/// Takes the lock, or goes a level deeper if this core has it.
///
/// Reading [`OWNER`] relaxed is enough for the "do I have it" test: only
/// a core stores its own id there, so seeing it means this core put it
/// there and hasn't let go.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_opl_lock() {
    let me = core_id() as u32;
    if OWNER.load(Ordering::Relaxed) == me {
        DEPTH.store(DEPTH.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        return;
    }
    while OWNER
        .compare_exchange_weak(NO_OWNER, me, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    DEPTH.store(1, Ordering::Relaxed);
}

/// Gives up one level of the lock, and the lock itself at the outermost.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_opl_unlock() {
    let depth = DEPTH.load(Ordering::Relaxed) - 1;
    DEPTH.store(depth, Ordering::Relaxed);
    if depth == 0 {
        OWNER.store(NO_OWNER, Ordering::Release);
    }
}

/// Resets the chip to power-on at the output rate, making it the first
/// time. Called under the lock, from `OPL_Init` on core 0: making the chip
/// allocates.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_opl_reset() {
    let mut chip = CHIP.load(Ordering::Relaxed);
    if chip.is_null() {
        chip = Box::into_raw(Box::new(Opl3Chip::default()));
        CHIP.store(chip, Ordering::Relaxed);
    }
    // Safety: under the lock, and the chip lives for the rest of the
    // program.
    opl3::reset(unsafe { &mut *chip }, sound::output_rate());
}

/// The rate the chip renders at: core 1's output rate.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_opl_rate() -> u32 {
    sound::output_rate()
}

/// Writes a chip register, `0x100` and up for the OPL3's second bank.
///
/// Through Nuked's write buffer, as Chocolate Doom's driver does: writes
/// that arrive together are spaced a couple of samples apart, the way a
/// real chip takes them.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_opl_write(reg: u32, value: u32) {
    // Safety: under the lock; null before `rpi_doom_opl_reset`.
    if let Some(chip) = unsafe { CHIP.load(Ordering::Relaxed).as_mut() } {
        opl3::write_reg_buffered(chip, reg as u16, value as u8);
    }
}

/// Renders `frames` stereo frames into `out`, interleaved.
///
/// # Safety
///
/// `out` has room for `2 * frames` samples. Called under the lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rpi_doom_opl_generate(out: *mut i16, frames: u32) {
    // Safety: the caller's guarantees.
    let out = unsafe { core::slice::from_raw_parts_mut(out, 2 * frames as usize) };
    match unsafe { CHIP.load(Ordering::Relaxed).as_mut() } {
        Some(chip) => {
            for frame in out.as_chunks_mut::<2>().0 {
                opl3::generate_resampled(chip, frame);
            }
        }
        None => out.fill(0),
    }
}

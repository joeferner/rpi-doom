#![no_std]
#![no_main]

//! rpi-doom, for the Raspberry Pi 2 (B).
//!
//! Boots on rpi-hal, starts rpi-kickstart's console on the UART
//! (GPIO14/15, 115200 8N1), mounts the SD card for the C library the
//! engine runs on (see `newlib`), sets up the display, and runs the
//! doomgeneric engine, which finds its WAD in `doom/` on the card.

extern crate alloc;

mod files;
mod newlib;
mod platform;

use core::ffi::{c_char, c_int};
use core::fmt::Write;

use alloc::boxed::Box;
use alloc::vec;
use rpi_hal::mailbox::{Mailbox, PixelOrder};
use rpi_hal::sd::{Sd, SdBlockDevice};
use rpi_hal::{halt, pac, timer::Timer, uart::Uart};
use rpi_kickstart::{console, heap, logln, storage};
use static_cell::StaticCell;

// The console borrows its sink for `'static`, so the UART can't live on
// `kmain`'s stack.
static UART: StaticCell<Uart> = StaticCell::new();

// The card's block device borrows the timer for as long as the card is
// mounted, which is the rest of the program.
static TIMER: StaticCell<Timer> = StaticCell::new();

/// The C heap: what newlib's `malloc` hands out, taken once from the
/// Rust heap. Doom's zone allocator asks for 6 MiB of it up front; the
/// rest is newlib's own buffers and the engine's smaller allocations.
const C_HEAP_BYTES: usize = 32 * 1024 * 1024;

/// The size to draw at when the firmware has no display mode to report,
/// e.g. when nothing was plugged in at boot.
const FALLBACK_WIDTH: u32 = 640;
const FALLBACK_HEIGHT: u32 = 480;

unsafe extern "C" {
    /// Starts the engine: parses `argv`, finds and loads the WAD, and
    /// runs it up to its first tic.
    fn doomgeneric_Create(argc: c_int, argv: *mut *mut c_char);
    /// Runs one tic of the game and draws it.
    fn doomgeneric_Tick();
}

/// The clock the console stamps each line with, in microseconds.
///
/// The System Timer's counters are read-only and free-running, so
/// stealing the peripheral here disturbs nothing.
fn uptime() -> u64 {
    Timer::new(unsafe { pac::Peripherals::steal() }.SYSTMR).now_micros()
}

// A program has exactly one panic handler, so it's the application's.
// It writes to a fresh UART rather than through the console: whatever
// owned the console may be what just panicked.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let peripherals = unsafe { pac::Peripherals::steal() };
    let mut uart = Uart::init(&peripherals.GPIO, peripherals.UART0);
    let _ = writeln!(uart, "PANIC: {info}");
    halt();
}

/// Where rpi-hal's boot code hands over, once the stack and `.bss` are
/// set up.
#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    let peripherals = unsafe { pac::Peripherals::steal() };
    let timer: &'static Timer = TIMER.init(Timer::new(peripherals.SYSTMR));

    console::init(
        UART.init(Uart::init(&peripherals.GPIO, peripherals.UART0)),
        uptime,
    );
    logln!("rpi-doom: hello from the Raspberry Pi 2 (B)");

    let mut mailbox = Mailbox::new(peripherals.VCMAILBOX);
    match heap::init(&mut mailbox) {
        Ok(bytes) => logln!("heap: {} MiB", bytes / (1024 * 1024)),
        Err(e) => {
            logln!("heap: {e:?}");
            halt();
        }
    }
    newlib::give_heap(Box::leak(vec![0u8; C_HEAP_BYTES].into_boxed_slice()));

    match Sd::init(&peripherals.GPIO, peripherals.EMMC, &mut mailbox, timer) {
        Ok(sd) => match storage::mount(SdBlockDevice::new(sd, timer)) {
            Ok(volume) => files::install(volume),
            Err(e) => logln!("card: {e}"),
        },
        Err(e) => logln!("card: no SD card ({e:?})"),
    }

    let (width, height) = match mailbox.display_mode() {
        Ok(mode) => (mode.width, mode.height),
        Err(e) => {
            logln!(
                "display: no mode from firmware ({e:?}), using {FALLBACK_WIDTH}x{FALLBACK_HEIGHT}"
            );
            (FALLBACK_WIDTH, FALLBACK_HEIGHT)
        }
    };
    // `Bgr` because a `0x00RRGGBB` word lands in memory on this
    // little-endian core as bytes `[BB, GG, RR, pad]`.
    match mailbox.allocate_framebuffer(width, height, 32, PixelOrder::Bgr) {
        Ok(framebuffer) => {
            logln!(
                "framebuffer: {}x{}, pitch {} bytes, at 0x{:08x}",
                framebuffer.width,
                framebuffer.height,
                framebuffer.pitch_bytes,
                framebuffer.address
            );
            if framebuffer.width < platform::WIDTH || framebuffer.height < platform::HEIGHT {
                logln!(
                    "framebuffer: smaller than Doom's {}x{} frame, not drawing",
                    platform::WIDTH,
                    platform::HEIGHT
                );
            } else {
                // Black, for the border around Doom's frame: the firmware
                // doesn't promise what a new buffer holds.
                // Safety: `size_bytes` is the buffer the firmware just
                // allocated.
                unsafe {
                    (framebuffer.address as *mut u8).write_bytes(0, framebuffer.size_bytes as usize)
                };
                framebuffer.flush();
                platform::show_on(framebuffer);
            }
        }
        Err(e) => logln!("framebuffer allocation failed: {e:?}"),
    }

    // `argv[0]` only: with no `-iwad`, the engine looks in its working
    // directory (the card's `doom/`) for each IWAD it knows by name.
    // `kmain` never returns, so the array outlives the engine's use of it.
    let mut argv = [c"doom".as_ptr().cast_mut(), core::ptr::null_mut()];
    // Safety: `argv` is NUL-terminated C strings and a null terminator,
    // and lives for the rest of the program.
    unsafe {
        doomgeneric_Create(1, argv.as_mut_ptr());
        loop {
            doomgeneric_Tick();
        }
    }
}

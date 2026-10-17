#![no_std]
#![no_main]

//! rpi-doom, for the Raspberry Pi 2 (B).
//!
//! Boots on rpi-hal, starts rpi-kickstart's console on the UART
//! (GPIO14/15, 115200 8N1), mounts the SD card for the C library the
//! engine runs on (see `newlib`), starts audio on core 1, sets up the
//! display and a USB keyboard, and runs the doomgeneric engine, which
//! finds its WAD in `doom/` on the card.

extern crate alloc;

mod audio;
mod files;
mod input;
mod music;
mod newlib;
mod opl3;
mod platform;
mod sound;

use core::ffi::{c_char, c_int};
use core::fmt::Write;

use alloc::boxed::Box;
use alloc::vec;
use rpi_hal::mailbox::{ClockId, Mailbox, PixelOrder};
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

/// How often the main loop logs core 1's chunk fill times.
const AUDIO_REPORT_US: u64 = 30_000_000;

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

    // The firmware starts the ARM at its minimum (600 MHz here) and leaves
    // it there until asked: under Linux the cpufreq governor asks. Without
    // this the game and core 1's music both run at two-thirds speed, and
    // nothing reports it (the throttle flags stay clear).
    match mailbox.max_clock_rate_hz(ClockId::Arm) {
        Ok(max) => match mailbox.set_clock_rate_hz(ClockId::Arm, max) {
            Ok(now) => logln!("arm: {} MHz", now / 1_000_000),
            Err(e) => logln!("arm: couldn't raise the clock ({e:?})"),
        },
        Err(e) => logln!("arm: no maximum clock rate ({e:?})"),
    }

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

    // After the card: see `audio::start`.
    // Safety: the only place the secondary cores are taken.
    let cores = unsafe { rpi_hal::multicore::Cores::steal() };
    sound::set_output_rate(audio::sample_rate_hz());
    audio::start(cores.core1);
    logln!("audio: core 1, {} Hz", audio::sample_rate_hz());

    input::init(
        peripherals.USB_OTG_GLOBAL,
        peripherals.USB_OTG_HOST,
        peripherals.USB_OTG_PWRCLK,
        &mut mailbox,
        timer,
    );

    // The display's shape, at roughly Doom's size: the firmware scales the
    // framebuffer to fill the display, which costs the CPU nothing, and
    // the shape keeps that scaling from distorting the frame (see
    // `platform::framebuffer_size`). Asking for the mode also clears any
    // overscan border, which would otherwise shrink the picture on a TV.
    let (width, height) = match mailbox.display_mode() {
        Ok(mode) => {
            logln!("display: {}x{}", mode.width, mode.height);
            platform::framebuffer_size(mode.width, mode.height)
        }
        Err(e) => {
            logln!("display: no mode from firmware ({e:?})");
            (platform::WIDTH, platform::HEIGHT)
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
                // Black, for the bars beside the frame: the firmware
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
        // Through startup, including the WAD load: a count well short of
        // the uptime's worth of chunks means the stream stopped.
        logln!("audio: {} chunks played", audio::chunks());
        let mut next_report = timer.now_micros();
        loop {
            doomgeneric_Tick();
            // How much of core 1's time a chunk takes, now music and
            // effects share it. A worst over the budget is an audible gap.
            if timer.now_micros() >= next_report {
                next_report += AUDIO_REPORT_US;
                let (average, worst, budget) = audio::fill_times();
                logln!("audio: chunk fill average {average} us, worst {worst} us, of {budget} us");
            }
        }
    }
}

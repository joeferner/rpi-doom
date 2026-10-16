#![no_std]
#![no_main]

//! rpi-doom, for the Raspberry Pi 2 (B).
//!
//! Boots on rpi-hal, starts rpi-kickstart's console on the UART
//! (GPIO14/15, 115200 8N1), draws a test pattern on the display, and logs
//! a heartbeat once a second. A count that keeps rising proves the board
//! is still running, not just that it reached `kmain`.

use core::fmt::Write;

use rpi_hal::mailbox::{Framebuffer, Mailbox, PixelOrder};
use rpi_hal::{halt, pac, timer::Timer, uart::Uart};
use rpi_kickstart::{console, logln};
use static_cell::StaticCell;

// The console borrows its sink for `'static`, so the UART can't live on
// `kmain`'s stack.
static UART: StaticCell<Uart> = StaticCell::new();

/// The size to draw at when the firmware has no display mode to report,
/// e.g. when nothing was plugged in at boot.
const FALLBACK_WIDTH: u32 = 640;
const FALLBACK_HEIGHT: u32 = 480;

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
    let timer = Timer::new(peripherals.SYSTMR);

    console::init(
        UART.init(Uart::init(&peripherals.GPIO, peripherals.UART0)),
        uptime,
    );
    logln!("rpi-doom: hello from the Raspberry Pi 2 (B)");

    let mut mailbox = Mailbox::new(peripherals.VCMAILBOX);
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
            draw_test_pattern(&framebuffer);
            framebuffer.flush();
        }
        Err(e) => logln!("framebuffer allocation failed: {e:?}"),
    }

    let mut seconds = 0u32;
    loop {
        timer.delay_ms(1000);
        seconds += 1;
        logln!("up {seconds}s");
    }
}

/// Fills the screen with eight vertical color bars inside a one-pixel
/// white border. The bars check the color channels, and the border
/// checks the whole buffer is visible: a side with no white line is
/// being cropped by the display.
fn draw_test_pattern(framebuffer: &Framebuffer) {
    const COLORS: [u32; 8] = [
        0x00FF_FFFF, // white
        0x00FF_FF00, // yellow
        0x0000_FFFF, // cyan
        0x0000_FF00, // green
        0x00FF_00FF, // magenta
        0x00FF_0000, // red
        0x0000_00FF, // blue
        0x0000_0000, // black
    ];
    const BORDER: u32 = 0x00FF_FFFF;

    let (width, height) = (framebuffer.width, framebuffer.height);
    let base = framebuffer.address as *mut u32;
    let pitch_pixels = framebuffer.pitch_bytes / 4;
    let bar_width = (width / COLORS.len() as u32).max(1);

    for y in 0..height {
        for x in 0..width {
            let color = if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
                BORDER
            } else {
                COLORS[(x / bar_width).min(COLORS.len() as u32 - 1) as usize]
            };
            // Safety: (x, y) is inside the buffer the firmware allocated
            // at this width, height and pitch.
            unsafe {
                base.add((y * pitch_pixels + x) as usize)
                    .write_volatile(color)
            };
        }
    }
}

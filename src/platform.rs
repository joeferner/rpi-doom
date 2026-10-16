//! doomgeneric's platform layer: the `DG_*` functions the engine calls to
//! show a frame, keep time and read keys.

use core::cell::Cell;
use core::ffi::{CStr, c_char, c_int, c_uchar};

use critical_section::Mutex;
use rpi_hal::mailbox::Framebuffer;
use rpi_hal::{pac, timer::Timer};
use rpi_kickstart::logln;

use crate::input;

/// The engine's frame size: doomgeneric's default `DOOMGENERIC_RESX` x
/// `DOOMGENERIC_RESY`, Doom's 320x200 doubled.
pub const WIDTH: u32 = 640;
/// See [`WIDTH`].
pub const HEIGHT: u32 = 400;

unsafe extern "C" {
    /// The frame the engine draws into: [`WIDTH`] x [`HEIGHT`] pixels,
    /// `0x00RRGGBB`, rows packed. doomgeneric allocates it before
    /// calling [`DG_Init`].
    static DG_ScreenBuffer: *const u32;
}

/// Where frames go. `None` until [`show_on`], and frames are dropped
/// until then.
static SCREEN: Mutex<Cell<Option<Framebuffer>>> = Mutex::new(Cell::new(None));

/// The framebuffer size to ask for on a `width` x `height` display: the
/// display's shape, just large enough to hold the [`WIDTH`] x [`HEIGHT`]
/// frame.
///
/// The firmware scales the framebuffer to fill the display, so giving it
/// the display's shape makes that scaling uniform and the frame keeps its
/// proportions. The frame fills the framebuffer along one axis and is
/// centered along the other, so the black bars are on two opposite
/// sides only: left and right on a display wider than 8:5 (800x480,
/// 16:9), top and bottom on a narrower one (4:3, 5:4), none on 16:10.
pub fn framebuffer_size(width: u32, height: u32) -> (u32, u32) {
    let (width, height) = (u64::from(width), u64::from(height));
    let (frame_width, frame_height) = (u64::from(WIDTH), u64::from(HEIGHT));
    if width * frame_height >= height * frame_width {
        // Wider than the frame: frame height, display shape.
        let scaled = (frame_height * width + height / 2) / height;
        (scaled as u32, HEIGHT)
    } else {
        // Narrower: frame width, display shape.
        let scaled = (frame_width * height + width / 2) / width;
        (WIDTH, scaled as u32)
    }
}

/// Sends the engine's frames to `framebuffer`, centered. It has to be at
/// least [`WIDTH`] x [`HEIGHT`] and 32 bits per pixel.
pub fn show_on(framebuffer: Framebuffer) {
    critical_section::with(|cs| SCREEN.borrow(cs).set(Some(framebuffer)));
}

/// Blanks the screen, for when the engine has stopped: otherwise its last
/// frame stays up, which looks like a hang.
pub fn blank() {
    let Some(screen) = critical_section::with(|cs| SCREEN.borrow(cs).get()) else {
        return;
    };
    // Safety: `size_bytes` is the whole buffer the firmware allocated.
    unsafe { (screen.address as *mut u8).write_bytes(0, screen.size_bytes as usize) };
    screen.flush();
}

/// The System Timer, for the clock functions. Its counters are read-only
/// and free-running, so stealing it disturbs nothing.
fn timer() -> Timer {
    Timer::new(unsafe { pac::Peripherals::steal() }.SYSTMR)
}

/// Called once, from `doomgeneric_Create`, before the engine starts.
/// Everything it would set up is ready before then.
#[unsafe(no_mangle)]
pub extern "C" fn DG_Init() {}

/// Copies the engine's finished frame onto the screen.
#[unsafe(no_mangle)]
pub extern "C" fn DG_DrawFrame() {
    let Some(screen) = critical_section::with(|cs| SCREEN.borrow(cs).get()) else {
        return;
    };
    // Centered; see `framebuffer_size`.
    let left = (screen.width - WIDTH) / 2;
    let top = (screen.height - HEIGHT) / 2;
    let pitch = screen.pitch_bytes as usize / 4;
    let base = screen.address as *mut u32;

    // Safety: `DG_ScreenBuffer` is the engine's `WIDTH * HEIGHT` frame,
    // and every destination row lies inside the framebuffer, which
    // `show_on` requires to be at least that large.
    unsafe {
        for y in 0..HEIGHT as usize {
            let from = DG_ScreenBuffer.add(y * WIDTH as usize);
            let to = base.add((top as usize + y) * pitch + left as usize);
            core::ptr::copy_nonoverlapping(from, to, WIDTH as usize);
        }
    }
    screen.flush();
}

/// Waits `ms` milliseconds.
#[unsafe(no_mangle)]
pub extern "C" fn DG_SleepMs(ms: u32) {
    timer().delay_ms(ms);
}

/// Milliseconds since boot. Wraps after 49 days, which the engine's tic
/// arithmetic tolerates.
#[unsafe(no_mangle)]
pub extern "C" fn DG_GetTicksMs() -> u32 {
    (timer().now_micros() / 1000) as u32
}

/// The next key event, if any: sets `*pressed` (1 down, 0 up) and `*key`
/// (a `doomkeys.h` code) and returns 1, or returns 0 when there is none.
/// The engine calls this until it returns 0, once per tic.
#[unsafe(no_mangle)]
pub extern "C" fn DG_GetKey(pressed: *mut c_int, key: *mut c_uchar) -> c_int {
    let Some(event) = input::next_key(&timer()) else {
        return 0;
    };
    // Safety: the engine passes pointers to its own locals.
    unsafe {
        *pressed = c_int::from(event.pressed);
        *key = event.code;
    }
    1
}

/// The engine names its "window" once it knows which game it's running.
#[unsafe(no_mangle)]
pub extern "C" fn DG_SetWindowTitle(title: *const c_char) {
    // Safety: the engine passes a NUL-terminated string.
    let title = unsafe { CStr::from_ptr(title) };
    logln!("doom: {}", title.to_str().unwrap_or("<not utf-8>"));
}

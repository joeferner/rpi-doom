//! The keyboard: a USB HID boot keyboard, read through rpi-hal's DWC2 host
//! driver and turned into the `doomkeys.h` key codes `DG_GetKey` hands
//! the engine.
//!
//! The keyboard has to be plugged in at boot. The bus is walked once, and
//! the first keyboard found is kept for the rest of the program.

use core::cell::RefCell;
use core::ops::ControlFlow;

use critical_section::Mutex;
use rpi_hal::mailbox::Mailbox;
use rpi_hal::pac::{USB_OTG_GLOBAL, USB_OTG_HOST, USB_OTG_PWRCLK};
use rpi_hal::timer::Timer;
use rpi_hal::usb::dwc2::{Channel, Dwc2Host};
use rpi_hal::usb::hid::keyboard::{KeyEvent, Keyboard, Modifiers};
use rpi_hal::usb::{self, Device};
use rpi_kickstart::logln;
use static_cell::StaticCell;

/// How often the keyboard is asked for a report. A boot keyboard's
/// interrupt endpoint is typically 10 ms, and polling it faster than its
/// interval upsets the controller's periodic scheduling.
const POLL_INTERVAL_US: u64 = 10_000;

/// How long to wait for the on-board hub to show up on the root port. It
/// is soldered on, so one that hasn't in this long isn't coming.
const HUB_WAIT_US: u64 = 5_000_000;

/// The most events one report can produce: six keys released, six
/// pressed, and Ctrl, Shift and Alt changing.
const MAX_EVENTS: usize = 15;

/// One key going down (`pressed`) or up, as a `doomkeys.h` code.
#[derive(Clone, Copy)]
pub struct Key {
    /// `true` for down, `false` for up.
    pub pressed: bool,
    /// The `doomkeys.h` code.
    pub code: u8,
}

struct State {
    keyboard: Keyboard,
    channel: Channel<'static>,
    /// When the keyboard was last asked for a report.
    last_poll_us: u64,
    /// The modifiers in the last report, since a report only lists
    /// non-modifier keys as presses and releases.
    modifiers: Modifiers,
    /// Events from the last report not yet handed to the engine.
    pending: [Key; MAX_EVENTS],
    pending_len: usize,
    next: usize,
    /// Whether the last poll failed, so a failure is logged once rather
    /// than every poll for as long as it lasts.
    failing: bool,
}

// Safety: `Channel` isn't `Send` because it borrows the `Dwc2Host`, which
// uses `Cell`s to hand out channels. This program runs on core 0 only and
// reaches the state only inside a critical section, so the host is never
// touched from two places at once.
unsafe impl Send for State {}

static STATE: Mutex<RefCell<Option<State>>> = Mutex::new(RefCell::new(None));

static DWC2: StaticCell<Dwc2Host> = StaticCell::new();

/// Powers the USB controller, walks the bus, and keeps the first keyboard
/// found. Without one, the game still runs; it just plays its demos.
pub fn init(
    global: USB_OTG_GLOBAL,
    host: USB_OTG_HOST,
    pwrclk: USB_OTG_PWRCLK,
    mailbox: &mut Mailbox,
    timer: &Timer,
) {
    if !usb::power_on(mailbox) {
        logln!("usb: power-on failed");
        return;
    }
    let dwc2: &'static Dwc2Host = DWC2.init(Dwc2Host::init(global, host, pwrclk, timer));

    let deadline = timer.now_micros() + HUB_WAIT_US;
    while !dwc2.port_connected() && timer.now_micros() < deadline {
        timer.delay_ms(100);
    }

    let mut found = None;
    let walked =
        usb::enumerate(
            dwc2,
            timer,
            |channel, timer, device: Device| match Keyboard::from_device(channel, timer, device) {
                Ok(Some(keyboard)) => {
                    logln!(
                        "usb: keyboard on hub {} port {}",
                        device.hub_address,
                        device.port
                    );
                    found = Some(keyboard);
                    ControlFlow::Break(())
                }
                Ok(None) => ControlFlow::Continue(()),
                Err(e) => {
                    logln!("usb: port {}: HID setup failed: {e:?}", device.port);
                    ControlFlow::Continue(())
                }
            },
        );
    if let Err(e) = walked {
        logln!("usb: enumeration failed: {e:?}");
    }
    let Some(keyboard) = found else {
        logln!("usb: no keyboard found; plug one in before booting");
        return;
    };
    let Some(channel) = dwc2.alloc_channel() else {
        logln!("usb: no free channel to poll the keyboard on");
        return;
    };

    critical_section::with(|cs| {
        *STATE.borrow_ref_mut(cs) = Some(State {
            keyboard,
            channel,
            last_poll_us: 0,
            modifiers: Modifiers(0),
            pending: [Key {
                pressed: false,
                code: 0,
            }; MAX_EVENTS],
            pending_len: 0,
            next: 0,
            failing: false,
        });
    });
}

/// The next key event, if there is one.
///
/// Hands out what the last report produced first; once those are gone,
/// asks the keyboard for a new report if [`POLL_INTERVAL_US`] has passed
/// since the last one.
pub fn next_key(timer: &Timer) -> Option<Key> {
    critical_section::with(|cs| {
        let mut state = STATE.borrow_ref_mut(cs);
        let state = state.as_mut()?;

        if state.next == state.pending_len {
            let now = timer.now_micros();
            if now - state.last_poll_us < POLL_INTERVAL_US {
                return None;
            }
            state.last_poll_us = now;
            state.poll(timer);
        }
        if state.next == state.pending_len {
            return None;
        }
        let key = state.pending[state.next];
        state.next += 1;
        Some(key)
    })
}

impl State {
    /// Asks the keyboard for a report and queues what changed.
    fn poll(&mut self, timer: &Timer) {
        self.pending_len = 0;
        self.next = 0;

        let events = match self.keyboard.poll(&mut self.channel, timer) {
            Ok(Some(events)) => events,
            Ok(None) => return,
            Err(e) => {
                if !self.failing {
                    logln!("usb: keyboard poll failed: {e:?}");
                    self.failing = true;
                }
                return;
            }
        };
        self.failing = false;

        let modifiers = events.report().modifiers;
        for (was, is, code) in [
            (self.modifiers.ctrl(), modifiers.ctrl(), KEY_RCTRL),
            (self.modifiers.shift(), modifiers.shift(), KEY_RSHIFT),
            (self.modifiers.alt(), modifiers.alt(), KEY_RALT),
        ] {
            if was != is {
                self.push(is, code);
            }
        }
        self.modifiers = modifiers;

        for event in events.iter() {
            let (pressed, usage) = match event {
                KeyEvent::Pressed(usage) => (true, usage),
                KeyEvent::Released(usage) => (false, usage),
            };
            if let Some(code) = doom_key(usage) {
                self.push(pressed, code);
            }
        }
    }

    fn push(&mut self, pressed: bool, code: u8) {
        if self.pending_len < MAX_EVENTS {
            self.pending[self.pending_len] = Key { pressed, code };
            self.pending_len += 1;
        }
    }
}

// The `doomkeys.h` codes that aren't plain ASCII.
const KEY_RIGHTARROW: u8 = 0xae;
const KEY_LEFTARROW: u8 = 0xac;
const KEY_UPARROW: u8 = 0xad;
const KEY_DOWNARROW: u8 = 0xaf;
const KEY_ESCAPE: u8 = 27;
const KEY_ENTER: u8 = 13;
const KEY_TAB: u8 = 9;
const KEY_BACKSPACE: u8 = 0x7f;
const KEY_PAUSE: u8 = 0xff;
const KEY_RSHIFT: u8 = 0x80 + 0x36;
const KEY_RCTRL: u8 = 0x80 + 0x1d;
const KEY_RALT: u8 = 0x80 + 0x38;
const KEY_F1: u8 = 0x80 + 0x3b;
const KEY_F11: u8 = 0x80 + 0x57;
const KEY_F12: u8 = 0x80 + 0x58;
const KEY_CAPSLOCK: u8 = 0x80 + 0x3a;
const KEY_SCRLCK: u8 = 0x80 + 0x46;
const KEY_PRTSCR: u8 = 0x80 + 0x59;
const KEY_HOME: u8 = 0x80 + 0x47;
const KEY_END: u8 = 0x80 + 0x4f;
const KEY_PGUP: u8 = 0x80 + 0x49;
const KEY_PGDN: u8 = 0x80 + 0x51;
const KEY_INS: u8 = 0x80 + 0x52;
const KEY_DEL: u8 = 0x80 + 0x53;

/// The `doomkeys.h` code for a HID keyboard usage, or `None` for a key
/// Doom has no use for.
///
/// Each key sends itself, never an action: Ctrl is `KEY_RCTRL`, not
/// `KEY_FIRE`, and Space is `' '`, not `KEY_USE`. What a key does is the
/// bindings' business (`key_fire`, `key_use`, ... in `default.cfg`),
/// which are saved as DOS scancodes and read back through `m_config.c`'s
/// `scantokey` table; every code here is one that table produces, so any
/// key can be bound and the binding survives a save. The engine's
/// defaults are vanilla's: Ctrl fires, Space uses, Shift runs, Alt
/// strafes, and `,` `.` strafe.
///
/// Letters are lowercase ASCII, which the menus and cheat codes expect.
/// The keypad sends what `doomkeys.h`'s `KEYP_*` names, which is what it
/// did with Num Lock off on DOS: arrows, Home/End/PgUp/PgDn, operators.
fn doom_key(usage: u8) -> Option<u8> {
    Some(match usage {
        // a-z
        0x04..=0x1d => b'a' + (usage - 0x04),
        // 1-9, then 0
        0x1e..=0x26 => b'1' + (usage - 0x1e),
        0x27 => b'0',
        0x28 => KEY_ENTER,
        0x29 => KEY_ESCAPE,
        0x2a => KEY_BACKSPACE,
        0x2b => KEY_TAB,
        0x2c => b' ',
        0x2d => b'-',
        0x2e => b'=',
        0x2f => b'[',
        0x30 => b']',
        0x31 => b'\\',
        0x33 => b';',
        0x34 => b'\'',
        0x35 => b'`',
        0x36 => b',',
        0x37 => b'.',
        0x38 => b'/',
        0x39 => KEY_CAPSLOCK,
        // F1-F10 are consecutive in both; F11 and F12 aren't.
        0x3a..=0x43 => KEY_F1 + (usage - 0x3a),
        0x44 => KEY_F11,
        0x45 => KEY_F12,
        0x46 => KEY_PRTSCR,
        0x47 => KEY_SCRLCK,
        0x48 => KEY_PAUSE,
        0x49 => KEY_INS,
        0x4a => KEY_HOME,
        0x4b => KEY_PGUP,
        0x4c => KEY_DEL,
        0x4d => KEY_END,
        0x4e => KEY_PGDN,
        0x4f => KEY_RIGHTARROW,
        0x50 => KEY_LEFTARROW,
        0x51 => KEY_DOWNARROW,
        0x52 => KEY_UPARROW,
        // Keypad. (Num Lock itself is left out: `scantokey` has no
        // scancode for it, so a binding to it couldn't be saved.)
        0x54 => b'/',
        0x55 => b'*',
        0x56 => b'-',
        0x57 => b'+',
        0x58 => KEY_ENTER,
        0x59 => KEY_END,
        0x5a => KEY_DOWNARROW,
        0x5b => KEY_PGDN,
        0x5c => KEY_LEFTARROW,
        0x5d => b'5',
        0x5e => KEY_RIGHTARROW,
        0x5f => KEY_HOME,
        0x60 => KEY_UPARROW,
        0x61 => KEY_PGUP,
        0x67 => b'=',
        _ => return None,
    })
}

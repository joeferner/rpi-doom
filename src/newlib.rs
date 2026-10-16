//! The OS layer under newlib: the handful of `_`-prefixed functions its C
//! library calls when it needs memory, the console or a file.
//!
//! newlib is portable because it never touches hardware itself. `malloc`
//! grows its heap with `_sbrk`, `printf` ends in `_write`, `fopen`/
//! `fread` end in `_open`/`_read`, and so on. On Linux those are system
//! calls; here they are these functions.

use core::cell::RefCell;
use core::ffi::{CStr, c_char, c_int, c_long, c_void};
use core::ptr;

use critical_section::Mutex;
use rpi_kickstart::logln;

use crate::files::{self, Errno};

unsafe extern "C" {
    /// newlib's `errno` lives in its reentrancy structure; this returns
    /// where.
    fn __errno() -> *mut c_int;
}

fn fail(errno: Errno) -> c_int {
    // Safety: `__errno` always returns a valid pointer to the current
    // reentrancy structure's `errno`.
    unsafe { *__errno() = errno as c_int };
    -1
}

/// The region `_sbrk` hands out, and how much of it is handed out.
struct Arena {
    base: *mut u8,
    len: usize,
    used: usize,
}

// Safety: the pointer is to memory owned by the arena for the rest of the
// program, and only ever reached under the critical section.
unsafe impl Send for Arena {}

static ARENA: Mutex<RefCell<Arena>> = Mutex::new(RefCell::new(Arena {
    base: ptr::null_mut(),
    len: 0,
    used: 0,
}));

/// Gives newlib's `malloc` its memory. Until this runs, every `malloc`
/// returns null.
pub fn give_heap(region: &'static mut [u8]) {
    critical_section::with(|cs| {
        *ARENA.borrow_ref_mut(cs) = Arena {
            base: region.as_mut_ptr(),
            len: region.len(),
            used: 0,
        };
    });
}

/// Grows the heap by `increment` bytes, returning the old end of the
/// heap.
#[unsafe(no_mangle)]
pub extern "C" fn _sbrk(increment: isize) -> *mut c_void {
    let grown = critical_section::with(|cs| {
        let arena = &mut *ARENA.borrow_ref_mut(cs);
        let used = arena.used.checked_add_signed(increment)?;
        if used > arena.len {
            return None;
        }
        let old_end = arena.base.wrapping_add(arena.used);
        arena.used = used;
        Some(old_end)
    });
    match grown {
        Some(old_end) => old_end.cast(),
        None => {
            fail(Errno::NoMemory);
            // `(void *)-1`, sbrk's failure value.
            usize::MAX as *mut c_void
        }
    }
}

/// Opens `path` with `flags` (see `files::open`). FAT has no permission
/// bits, so `mode` goes unused.
#[unsafe(no_mangle)]
pub extern "C" fn _open(path: *const c_char, flags: c_int, _mode: c_int) -> c_int {
    // Safety: newlib passes the caller's NUL-terminated path.
    let path = unsafe { CStr::from_ptr(path) };
    files::open(path.to_bytes(), flags).unwrap_or_else(fail)
}

/// Reads up to `len` bytes from `fd` into `buf`.
#[unsafe(no_mangle)]
pub extern "C" fn _read(fd: c_int, buf: *mut c_char, len: c_int) -> c_int {
    if (0..=2).contains(&fd) {
        // No keyboard on the console: stdin is always at its end.
        return 0;
    }
    let Ok(len) = usize::try_from(len) else {
        return fail(Errno::Invalid);
    };
    // Safety: newlib passes a buffer of at least `len` bytes.
    let into = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), len) };
    match files::read(fd, into) {
        Ok(read) => read as c_int,
        Err(errno) => fail(errno),
    }
}

/// Writes `len` bytes of `buf` to `fd`: stdout and stderr go to the
/// console, anything from 3 up to a file on the card.
#[unsafe(no_mangle)]
pub extern "C" fn _write(fd: c_int, buf: *const c_char, len: c_int) -> c_int {
    let Ok(len) = usize::try_from(len) else {
        return fail(Errno::Invalid);
    };
    // Safety: newlib passes a buffer of at least `len` bytes.
    let bytes = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), len) };
    match fd {
        1 | 2 => {
            console_write(bytes);
            len as c_int
        }
        3.. => match files::write(fd, bytes) {
            Ok(written) => written as c_int,
            Err(errno) => fail(errno),
        },
        _ => fail(Errno::BadFd),
    }
}

/// Moves `fd`'s position.
#[unsafe(no_mangle)]
pub extern "C" fn _lseek(fd: c_int, offset: c_long, whence: c_int) -> c_long {
    if (0..=2).contains(&fd) {
        return fail(Errno::NotSeekable) as c_long;
    }
    match files::seek(fd, i64::from(offset), whence) {
        Ok(position) => {
            c_long::try_from(position).unwrap_or_else(|_| fail(Errno::Invalid) as c_long)
        }
        Err(errno) => fail(errno) as c_long,
    }
}

/// Closes `fd`. Closing the console is allowed and does nothing.
#[unsafe(no_mangle)]
pub extern "C" fn _close(fd: c_int) -> c_int {
    if (0..=2).contains(&fd) {
        return 0;
    }
    match files::close(fd) {
        Ok(()) => 0,
        Err(errno) => fail(errno),
    }
}

/// newlib's `struct stat` for `arm-none-eabi`, as far as `_fstat` fills
/// it in: 88 bytes, `st_mode` (`mode_t`, 32-bit) at offset 4 and
/// `st_size` (`off_t`, a 32-bit `long`) at offset 16. Measured from
/// newlib's `<sys/stat.h>` with `offsetof`; the fields between are
/// zeroed.
#[repr(C)]
pub struct Stat {
    _dev_ino: [u16; 2],
    mode: u32,
    _nlink_uid_gid_rdev: [u16; 4],
    size: c_long,
    _rest: [u8; 68],
}

const _: () = assert!(size_of::<Stat>() == 88);
const _: () = assert!(core::mem::offset_of!(Stat, mode) == 4);
const _: () = assert!(core::mem::offset_of!(Stat, size) == 16);

/// Describes `fd`. The console descriptors are character devices, which
/// is what makes newlib ask [`_isatty`] and line-buffer stdout; anything
/// else is a regular file of the length the card reports.
#[unsafe(no_mangle)]
pub extern "C" fn _fstat(fd: c_int, stat: *mut Stat) -> c_int {
    const S_IFCHR: u32 = 0o020000;
    const S_IFREG: u32 = 0o100000;

    let (mode, size) = if (0..=2).contains(&fd) {
        (S_IFCHR, 0)
    } else {
        match files::len(fd) {
            Ok(len) => (S_IFREG, c_long::try_from(len).unwrap_or(c_long::MAX)),
            Err(errno) => return fail(errno),
        }
    };
    // Safety: newlib passes a pointer to a whole `struct stat`, which
    // `Stat` matches in size.
    unsafe {
        stat.write_bytes(0, 1);
        (*stat).mode = mode;
        (*stat).size = size;
    }
    0
}

/// Whether `fd` is a terminal: the console descriptors are, which is what
/// makes newlib line-buffer stdout.
#[unsafe(no_mangle)]
pub extern "C" fn _isatty(fd: c_int) -> c_int {
    c_int::from((0..=2).contains(&fd))
}

/// Ends the program: Quit Game from the menu, or `I_Error`. There is
/// nothing to return to, so it blanks the screen and stops here.
#[unsafe(no_mangle)]
pub extern "C" fn _exit(status: c_int) -> ! {
    logln!("exit({status})");
    crate::platform::blank();
    rpi_hal::halt();
}

/// Run by `exit` after the C destructors, as the last step of shutting
/// down. There is nothing left to tear down.
#[unsafe(no_mangle)]
pub extern "C" fn _fini() {}

/// Making directories isn't supported; refused. Doom asks for its config
/// directory, which is the working directory (`""`), and carries on.
///
/// Not `_mkdir`: newlib has no `mkdir` of its own to wrap one.
#[unsafe(no_mangle)]
pub extern "C" fn mkdir(_path: *const c_char, _mode: c_int) -> c_int {
    fail(Errno::ReadOnly)
}

/// Deletes a file; what newlib's `remove` calls.
#[unsafe(no_mangle)]
pub extern "C" fn _unlink(path: *const c_char) -> c_int {
    // Safety: newlib passes the caller's NUL-terminated path.
    let path = unsafe { CStr::from_ptr(path) };
    match files::remove(path.to_bytes()) {
        Ok(()) => 0,
        Err(errno) => fail(errno),
    }
}

/// Renames `old` to `new`, replacing a file at `new`.
///
/// newlib's own `rename` is `_link` then `_unlink`, which FAT can't do:
/// it has no hard links. Defining `rename` here keeps newlib's out of the
/// link, and resident-fat renames in one step.
#[unsafe(no_mangle)]
pub extern "C" fn rename(old: *const c_char, new: *const c_char) -> c_int {
    // Safety: the caller passes NUL-terminated paths.
    let (old, new) = unsafe { (CStr::from_ptr(old), CStr::from_ptr(new)) };
    match files::rename(old.to_bytes(), new.to_bytes()) {
        Ok(()) => 0,
        Err(errno) => fail(errno),
    }
}

/// There are no other processes to signal.
#[unsafe(no_mangle)]
pub extern "C" fn _kill(_pid: c_int, _signal: c_int) -> c_int {
    fail(Errno::Invalid)
}

/// The only process there is.
#[unsafe(no_mangle)]
pub extern "C" fn _getpid() -> c_int {
    1
}

/// The console takes whole lines, and `printf` writes in pieces, so this
/// gathers bytes until a newline (or a full buffer) and logs the line.
fn console_write(bytes: &[u8]) {
    const LINE: usize = 160;
    static PENDING: Mutex<RefCell<([u8; LINE], usize)>> = Mutex::new(RefCell::new(([0; LINE], 0)));

    critical_section::with(|cs| {
        let (line, len) = &mut *PENDING.borrow_ref_mut(cs);
        for &byte in bytes {
            if byte == b'\n' || *len == LINE {
                logln!(
                    "{}",
                    core::str::from_utf8(&line[..*len]).unwrap_or("<not utf-8>")
                );
                *len = 0;
                if byte == b'\n' {
                    continue;
                }
            }
            line[*len] = byte;
            *len += 1;
        }
    });
}

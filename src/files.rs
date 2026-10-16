//! The files newlib's `open`/`read`/`lseek`/`close` reach: an open-file
//! table over the FAT volume on the SD card.
//!
//! Read-only for now. Doom reads its WAD and config through here; opening
//! anything for writing fails with `EROFS`, which Doom treats as "no
//! config saved" rather than an error.
//!
//! # Paths
//!
//! Doom opens files by relative path (`./doom1.wad`, `default.cfg`), as
//! if from a working directory. That directory is [`HOME`] on the card,
//! so the WAD, config and saves sit together rather than among the boot
//! firmware in the root. A path starting with `/` is taken from the root
//! of the card instead.

use core::cell::RefCell;

use critical_section::Mutex;
use resident_fat::File;
use rpi_hal::sd::SdBlockDevice;
use rpi_kickstart::storage::Volume;

/// The card directory relative paths resolve against.
pub const HOME: &str = "doom";

/// How many files can be open at once, not counting stdin/stdout/stderr.
/// Doom keeps the WAD open and briefly opens a config file or two.
const MAX_OPEN: usize = 8;

/// File descriptors 0-2 are the console; the table starts after them.
const FIRST_FD: i32 = 3;

/// The longest path accepted, after joining it onto [`HOME`].
const MAX_PATH: usize = 128;

/// Why an operation failed, as the `errno` value newlib expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Errno {
    /// No such file.
    NoEntry = 2,
    /// The card reported a read error.
    Io = 5,
    /// Not an open file descriptor.
    BadFd = 9,
    /// The C heap is used up.
    NoMemory = 12,
    /// A bad argument: a negative seek, an unknown `whence`.
    Invalid = 22,
    /// Every slot in the table is in use.
    TooManyOpen = 24,
    /// A seek on the console.
    NotSeekable = 29,
    /// A write was asked for; nothing here writes yet.
    ReadOnly = 30,
}

struct Open {
    file: File,
    position: u64,
}

struct Table {
    volume: Option<Volume<SdBlockDevice<'static>>>,
    open: [Option<Open>; MAX_OPEN],
}

// Safety: the volume isn't `Send` because it holds a `&Timer` and a boxed
// clock, neither of which says it may cross threads. This program has one:
// only core 0 runs, and the table is only reached inside a critical
// section, so nothing in it is ever touched from two places at once.
unsafe impl Send for Table {}

static TABLE: Mutex<RefCell<Table>> = Mutex::new(RefCell::new(Table {
    volume: None,
    open: [const { None }; MAX_OPEN],
}));

/// Hands the mounted card to the table. Until this runs, every `open`
/// fails with [`Errno::NoEntry`].
pub fn install(volume: Volume<SdBlockDevice<'static>>) {
    critical_section::with(|cs| TABLE.borrow_ref_mut(cs).volume = Some(volume));
}

/// Opens `path` for reading, returning its file descriptor.
pub fn open(path: &[u8]) -> Result<i32, Errno> {
    let mut joined = [0u8; MAX_PATH];
    let path = resolve(path, &mut joined)?;
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        let volume = table.volume.as_mut().ok_or(Errno::NoEntry)?;
        let slot = table
            .open
            .iter()
            .position(Option::is_none)
            .ok_or(Errno::TooManyOpen)?;
        let file = volume.open(path).map_err(|e| match e {
            resident_fat::Error::NotFound { .. } | resident_fat::Error::IsADirectory { .. } => {
                Errno::NoEntry
            }
            _ => Errno::Io,
        })?;
        table.open[slot] = Some(Open { file, position: 0 });
        Ok(FIRST_FD + slot as i32)
    })
}

/// Reads from the file's current position into `into`, advancing it.
/// Returns 0 at the end of the file.
pub fn read(fd: i32, into: &mut [u8]) -> Result<usize, Errno> {
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        let open = slot(&mut table.open, fd)?;
        let volume = table.volume.as_mut().ok_or(Errno::BadFd)?;
        let read = volume
            .read_at(&open.file, open.position, into)
            .map_err(|_| Errno::Io)?;
        open.position += read as u64;
        Ok(read)
    })
}

/// Moves the file's position, `lseek` style: `whence` is 0 (from the
/// start), 1 (from the current position) or 2 (from the end). Returns the
/// new position.
pub fn seek(fd: i32, offset: i64, whence: i32) -> Result<u64, Errno> {
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        let open = slot(&mut table.open, fd)?;
        let base = match whence {
            0 => 0,
            1 => open.position as i64,
            2 => i64::from(open.file.len()),
            _ => return Err(Errno::Invalid),
        };
        let position = u64::try_from(base + offset).map_err(|_| Errno::Invalid)?;
        open.position = position;
        Ok(position)
    })
}

/// The file's length in bytes.
pub fn len(fd: i32) -> Result<u32, Errno> {
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        Ok(slot(&mut table.open, fd)?.file.len())
    })
}

/// Closes the file, freeing its descriptor.
pub fn close(fd: i32) -> Result<(), Errno> {
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        slot(&mut table.open, fd)?;
        table.open[(fd - FIRST_FD) as usize] = None;
        Ok(())
    })
}

fn slot(open: &mut [Option<Open>; MAX_OPEN], fd: i32) -> Result<&mut Open, Errno> {
    let index = usize::try_from(fd - FIRST_FD).map_err(|_| Errno::BadFd)?;
    open.get_mut(index)
        .and_then(Option::as_mut)
        .ok_or(Errno::BadFd)
}

/// Turns a path as Doom gives it into one on the card: `./` prefixes
/// dropped, relative paths joined onto [`HOME`], a leading `/` taken as
/// the card's root.
fn resolve<'a>(path: &[u8], joined: &'a mut [u8; MAX_PATH]) -> Result<&'a str, Errno> {
    let mut path = path;
    while let Some(rest) = path.strip_prefix(b"./") {
        path = rest;
    }
    let len = if let Some(absolute) = path.strip_prefix(b"/") {
        copy(joined, 0, absolute)?
    } else {
        let at = copy(joined, 0, HOME.as_bytes())?;
        let at = copy(joined, at, b"/")?;
        copy(joined, at, path)?
    };
    core::str::from_utf8(&joined[..len]).map_err(|_| Errno::NoEntry)
}

fn copy(into: &mut [u8; MAX_PATH], at: usize, bytes: &[u8]) -> Result<usize, Errno> {
    let end = at + bytes.len();
    into.get_mut(at..end)
        .ok_or(Errno::NoEntry)?
        .copy_from_slice(bytes);
    Ok(end)
}

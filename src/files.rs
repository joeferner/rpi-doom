//! The files newlib's `open`/`read`/`write`/`lseek`/`close` reach: an
//! open-file table over the FAT volume on the SD card.
//!
//! Files can be read, created, truncated, written, deleted and renamed. A
//! file that was written is synced when it's closed: resident-fat keeps
//! allocation-table changes in memory until then, so closing is what puts
//! a written file on the card for good. Deleting and renaming sync
//! straight away. Making directories isn't here: nothing needs it, since
//! the config and saves go beside the WAD.
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
use rpi_kickstart::logln;
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

// newlib's `open` flags for `arm-none-eabi`, from its `<sys/_default_fcntl.h>`.
const O_ACCMODE: i32 = 3;
const O_APPEND: i32 = 0x0008;
const O_CREAT: i32 = 0x0200;
const O_TRUNC: i32 = 0x0400;
const O_EXCL: i32 = 0x0800;

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
    /// `O_CREAT | O_EXCL` on a file that's already there.
    Exists = 17,
    /// A bad argument: a negative seek, an unknown `whence`.
    Invalid = 22,
    /// Every slot in the table is in use.
    TooManyOpen = 24,
    /// The card is full.
    NoSpace = 28,
    /// A seek on the console.
    NotSeekable = 29,
    /// An operation that isn't supported (making directories), refused as
    /// if the card were read-only.
    ReadOnly = 30,
}

impl<E: core::fmt::Debug> From<resident_fat::Error<E>> for Errno {
    fn from(error: resident_fat::Error<E>) -> Self {
        match error {
            resident_fat::Error::NotFound { .. }
            | resident_fat::Error::NotADirectory { .. }
            | resident_fat::Error::IsADirectory { .. } => Errno::NoEntry,
            resident_fat::Error::AlreadyExists { .. } => Errno::Exists,
            resident_fat::Error::Fat(resident_fat::FatError::DiskFull { .. })
            | resident_fat::Error::DirectoryFull => Errno::NoSpace,
            resident_fat::Error::OutOfMemory { .. } => Errno::NoMemory,
            // Everything else is `EIO`, which says nothing about why;
            // the console line does.
            error => {
                logln!("card: {error}");
                Errno::Io
            }
        }
    }
}

struct Open {
    file: File,
    position: u64,
    /// Opened for writing (`O_WRONLY` or `O_RDWR`).
    writable: bool,
    /// `O_APPEND`: every write goes at the end, wherever the position is.
    append: bool,
    /// Written since it was opened, so closing it has to sync.
    written: bool,
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

/// Opens `path` with newlib's `open` `flags`, returning its file
/// descriptor. `O_CREAT` creates a missing file, `O_TRUNC` empties an
/// existing one opened for writing, and `O_EXCL` with `O_CREAT` refuses
/// one that's already there.
pub fn open(path: &[u8], flags: i32) -> Result<i32, Errno> {
    let mut joined = [0u8; MAX_PATH];
    let path = resolve(path, &mut joined)?;
    let writable = flags & O_ACCMODE != 0;
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        let volume = table.volume.as_mut().ok_or(Errno::NoEntry)?;
        let slot = table
            .open
            .iter()
            .position(Option::is_none)
            .ok_or(Errno::TooManyOpen)?;

        let mut written = false;
        let file = match volume.open(path) {
            Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(Errno::Exists),
            Ok(mut file) => {
                if writable && flags & O_TRUNC != 0 && !file.is_empty() {
                    volume.truncate(&mut file, 0)?;
                    written = true;
                }
                file
            }
            Err(resident_fat::Error::NotFound { .. }) if flags & O_CREAT != 0 => {
                written = true;
                volume.create(path)?
            }
            Err(e) => return Err(e.into()),
        };
        table.open[slot] = Some(Open {
            file,
            position: 0,
            writable,
            append: flags & O_APPEND != 0,
            written,
        });
        Ok(FIRST_FD + slot as i32)
    })
}

/// Writes `data` at the file's current position (or its end, for
/// `O_APPEND`), advancing the position and growing the file as needed.
pub fn write(fd: i32, data: &[u8]) -> Result<usize, Errno> {
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        let open = slot(&mut table.open, fd)?;
        if !open.writable {
            return Err(Errno::BadFd);
        }
        let volume = table.volume.as_mut().ok_or(Errno::BadFd)?;
        // Marked before the write, so a write that fails partway still
        // gets its allocation-table changes synced on close.
        open.written = true;
        if open.append {
            open.position = u64::from(open.file.len());
        }
        volume.write_at(&mut open.file, open.position, data)?;
        open.position += data.len() as u64;
        Ok(data.len())
    })
}

/// Reads from the file's current position into `into`, advancing it.
/// Returns 0 at the end of the file.
pub fn read(fd: i32, into: &mut [u8]) -> Result<usize, Errno> {
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        let open = slot(&mut table.open, fd)?;
        let volume = table.volume.as_mut().ok_or(Errno::BadFd)?;
        let read = volume.read_at(&open.file, open.position, into)?;
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

/// Deletes the file at `path`, and syncs the card.
pub fn remove(path: &[u8]) -> Result<(), Errno> {
    let mut joined = [0u8; MAX_PATH];
    let path = resolve(path, &mut joined)?;
    critical_section::with(|cs| {
        let volume = &mut *TABLE.borrow_ref_mut(cs);
        let volume = volume.volume.as_mut().ok_or(Errno::NoEntry)?;
        volume.remove(path)?;
        volume.sync()?;
        Ok(())
    })
}

/// Renames `from` to `to`, replacing a file already at `to`, and syncs
/// the card.
///
/// resident-fat replaces in an order that leaves `to` naming a whole
/// file, the old one or the new, whenever it's interrupted. That's what
/// Doom relies on when it saves a game: it writes `temp.dsg` and renames
/// it over the slot's file.
pub fn rename(from: &[u8], to: &[u8]) -> Result<(), Errno> {
    let mut joined_from = [0u8; MAX_PATH];
    let mut joined_to = [0u8; MAX_PATH];
    let from = resolve(from, &mut joined_from)?;
    let to = resolve(to, &mut joined_to)?;
    critical_section::with(|cs| {
        let volume = &mut *TABLE.borrow_ref_mut(cs);
        let volume = volume.volume.as_mut().ok_or(Errno::NoEntry)?;
        volume.rename(from, to)?;
        volume.sync()?;
        Ok(())
    })
}

/// Closes the file, freeing its descriptor, and syncs the card if it was
/// written. The descriptor is freed even if the sync fails.
pub fn close(fd: i32) -> Result<(), Errno> {
    critical_section::with(|cs| {
        let table = &mut *TABLE.borrow_ref_mut(cs);
        let written = slot(&mut table.open, fd)?.written;
        table.open[(fd - FIRST_FD) as usize] = None;
        if written {
            table.volume.as_mut().ok_or(Errno::BadFd)?.sync()?;
        }
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

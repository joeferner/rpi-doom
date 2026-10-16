//! The sound-effects mixer: channel state core 0 sets, mixed on core 1.
//!
//! The engine's sound module (c/i_rpisound.c) calls the `rpi_doom_sound_*`
//! functions on core 0 to start, adjust and stop a sound on a channel.
//! Core 1 ([`Mixer::mix`], from src/audio.rs) reads that state a chunk at
//! a time, plays each channel's samples from its own private position, and
//! sums them.
//!
//! # Handing a sound across
//!
//! Core 0 and core 1 share nothing but atomics: core 1's fill loop
//! mustn't wait on core 0, and with rpi-hal's `multicore` feature a
//! critical section is a lock between the two.
//!
//! A sound is three words (samples, length, step), and core 1 has to see
//! all three from the same `start`, never a mix of two. Each channel has a
//! sequence number for that, a seqlock: odd while core 0 is writing the
//! three, even once they're whole. Core 1 reads the number, the three
//! words, and the number again, and only takes the words if the number was
//! even and didn't move. A new even number is a new sound, and core 1
//! starts it from the beginning.
//!
//! The volume and stereo position change while a sound plays
//! (`UpdateSoundParams`, as the source moves), and never restart it. They
//! are one word, both gains packed, so core 1 always sees a matching pair
//! without the seqlock.
//!
//! When a sound ends, core 1 records its sequence number as done. A
//! channel is playing while its latest sound isn't done.
//!
//! # The samples
//!
//! Unsigned 8-bit mono at the lump's own rate (mostly 11,025 Hz), read in
//! place from the engine's lump cache. Core 1 steps through them at
//! `rate / output rate` per output frame in 16.16 fixed point, and
//! interpolates linearly between neighbours.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering, fence};

use rpi_hal::multicore::CacheAligned;

/// Channels the mixer has. The engine uses `snd_channels` of them (8 by
/// default, the most the config allows).
const CHANNELS: usize = 16;

/// One channel's sound, as core 0 publishes it.
struct Channel {
    /// The seqlock: odd while core 0 is writing [`samples`](Self::samples),
    /// [`len`](Self::len) and [`step`](Self::step). Bumped by every start
    /// and stop.
    seq: AtomicU32,
    /// Address of the first sample.
    samples: AtomicUsize,
    /// Samples in the sound. Zero for "stopped".
    len: AtomicU32,
    /// Input samples per output frame, 16.16 fixed point.
    step: AtomicU32,
    /// Left gain in the high half, right in the low, each 0-255.
    gains: AtomicU32,
    /// The sequence number of the latest sound core 1 has finished or
    /// seen stopped. Written only by core 1.
    done: AtomicU32,
}

impl Channel {
    const fn new() -> Self {
        Channel {
            seq: AtomicU32::new(0),
            samples: AtomicUsize::new(0),
            len: AtomicU32::new(0),
            step: AtomicU32::new(0),
            gains: AtomicU32::new(0),
            done: AtomicU32::new(0),
        }
    }
}

/// Each channel on its own cache line: core 0 writes one channel while
/// core 1 mixes the rest, and sharing a line would make every write
/// contend with core 1's reads.
static CHANNELS_STATE: [CacheAligned<Channel>; CHANNELS] =
    [const { CacheAligned(Channel::new()) }; CHANNELS];

/// The rate core 1 plays at, for working out each sound's step. Set once
/// at boot, before core 0 can start a sound.
static OUTPUT_RATE: AtomicU32 = AtomicU32::new(0);

/// Records the output rate. Call before the engine starts.
pub fn set_output_rate(hz: u32) {
    OUTPUT_RATE.store(hz, Ordering::Relaxed);
}

fn channel(index: i32) -> Option<&'static Channel> {
    usize::try_from(index)
        .ok()
        .and_then(|i| CHANNELS_STATE.get(i))
        .map(|c| &c.0)
}

fn pack_gains(left: i32, right: i32) -> u32 {
    (left.clamp(0, 255) as u32) << 16 | right.clamp(0, 255) as u32
}

/// Publishes a new sound on `channel`, under its seqlock. Core 0 only.
fn publish(channel: &Channel, samples: usize, len: u32, step: u32) {
    let seq = channel.seq.load(Ordering::Relaxed);
    // Odd: core 1 leaves the channel's words alone until it's even again.
    channel.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    channel.samples.store(samples, Ordering::Relaxed);
    channel.len.store(len, Ordering::Relaxed);
    channel.step.store(step, Ordering::Relaxed);
    channel.seq.store(seq.wrapping_add(2), Ordering::Release);
}

/// The channels the engine may use.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_sound_channels() -> i32 {
    CHANNELS as i32
}

/// Starts `len` samples at `rate` Hz on `channel`, replacing whatever it
/// was playing, at gains `left` and `right` (0-255).
///
/// # Safety
///
/// `samples` points at `len` bytes that stay put for as long as the
/// channel could be playing them: the engine's PU_STATIC lump cache.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rpi_doom_sound_start(
    channel: i32,
    samples: *const u8,
    len: u32,
    rate: u32,
    left: i32,
    right: i32,
) {
    let Some(channel) = self::channel(channel) else {
        return;
    };
    let output = OUTPUT_RATE.load(Ordering::Relaxed);
    if output == 0 || len == 0 {
        return;
    }
    let step = ((u64::from(rate) << 16) / u64::from(output)) as u32;
    channel
        .gains
        .store(pack_gains(left, right), Ordering::Relaxed);
    publish(channel, samples as usize, len, step);
}

/// Changes the gains of whatever `channel` is playing, without restarting
/// it.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_sound_params(channel: i32, left: i32, right: i32) {
    if let Some(channel) = self::channel(channel) {
        channel
            .gains
            .store(pack_gains(left, right), Ordering::Relaxed);
    }
}

/// Stops `channel`.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_sound_stop(channel: i32) {
    if let Some(channel) = self::channel(channel) {
        publish(channel, 0, 0, 0);
    }
}

/// Whether `channel`'s latest sound is still playing: started, and not
/// yet finished or stopped.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_doom_sound_playing(channel: i32) -> i32 {
    let Some(channel) = self::channel(channel) else {
        return 0;
    };
    let seq = channel.seq.load(Ordering::Relaxed);
    let playing =
        channel.len.load(Ordering::Relaxed) != 0 && channel.done.load(Ordering::Acquire) != seq;
    i32::from(playing)
}

/// What core 1 is playing on one channel: its own copy of the sound and
/// its position in it.
#[derive(Clone, Copy)]
struct Voice {
    /// The sequence number this voice's sound came from.
    seq: u32,
    samples: *const u8,
    len: u32,
    step: u32,
    /// Position in the samples, 16.16 fixed point. 64 bits: a long sound
    /// times 65536 doesn't fit in 32.
    position: u64,
    /// Whether there's anything left to play.
    active: bool,
}

/// Core 1's side: one [`Voice`] per channel.
pub struct Mixer {
    voices: [Voice; CHANNELS],
}

// Safety: a `Mixer` lives on core 1 and is only ever used there; the
// pointers in it are to the engine's lump cache, which doesn't move.
unsafe impl Send for Mixer {}

impl Mixer {
    /// A mixer with nothing playing.
    pub const fn new() -> Self {
        Mixer {
            voices: [Voice {
                seq: 0,
                samples: core::ptr::null(),
                len: 0,
                step: 0,
                position: 0,
                active: false,
            }; CHANNELS],
        }
    }

    /// Mixes every channel into `out`, overwriting it, as signed 16-bit
    /// stereo frames.
    pub fn mix(&mut self, out: &mut [[i16; 2]]) {
        let mut sums = [[0i32; 2]; 512];
        let sums = &mut sums[..out.len()];

        for (voice, shared) in self.voices.iter_mut().zip(&CHANNELS_STATE) {
            let shared = &shared.0;
            voice.pick_up(shared);
            if !voice.active {
                continue;
            }
            let gains = shared.gains.load(Ordering::Relaxed);
            let (left, right) = ((gains >> 16) as i32, (gains & 0xffff) as i32);
            for sum in sums.iter_mut() {
                let Some(sample) = voice.next() else {
                    // Finished: tell core 0, which may be waiting for it in
                    // `SoundIsPlaying`.
                    voice.active = false;
                    shared.done.store(voice.seq, Ordering::Release);
                    break;
                };
                // 8-bit sample times a 0-255 gain: full gain is full 16-bit
                // scale.
                sum[0] += sample * left;
                sum[1] += sample * right;
            }
        }

        for (frame, sum) in out.iter_mut().zip(sums.iter()) {
            frame[0] = sum[0].clamp(i16::MIN.into(), i16::MAX.into()) as i16;
            frame[1] = sum[1].clamp(i16::MIN.into(), i16::MAX.into()) as i16;
        }
    }
}

impl Voice {
    /// Takes up a new sound if core 0 has published one, under the
    /// channel's seqlock (see this module's header).
    fn pick_up(&mut self, shared: &Channel) {
        let seq = shared.seq.load(Ordering::Acquire);
        if seq == self.seq || seq & 1 != 0 {
            return;
        }
        let samples = shared.samples.load(Ordering::Relaxed);
        let len = shared.len.load(Ordering::Relaxed);
        let step = shared.step.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        if shared.seq.load(Ordering::Relaxed) != seq {
            // Core 0 started writing again; the words may be torn. Next
            // chunk.
            return;
        }
        *self = Voice {
            seq,
            samples: samples as *const u8,
            len,
            step,
            position: 0,
            active: len != 0,
        };
        if !self.active {
            // A stop: nothing to play, and already done.
            shared.done.store(seq, Ordering::Release);
        }
    }

    /// The next output sample, centred on zero (-128..=127), or `None`
    /// once the sound has run out.
    fn next(&mut self) -> Option<i32> {
        let index = (self.position >> 16) as u32;
        if index >= self.len {
            return None;
        }
        // Safety: `index < len`, and the sound's `len` bytes stay put while
        // it plays (see `rpi_doom_sound_start`).
        let here = i32::from(unsafe { *self.samples.add(index as usize) });
        let next = if index + 1 < self.len {
            i32::from(unsafe { *self.samples.add(index as usize + 1) })
        } else {
            here
        };
        let fraction = (self.position & 0xffff) as i32;
        let sample = here + (((next - here) * fraction) >> 16);
        self.position += u64::from(self.step);
        Some(sample - 128)
    }
}

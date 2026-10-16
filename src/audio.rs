//! Sound out of the 3.5 mm jack, from core 1.
//!
//! Core 1 owns the output outright: the PWM block, its clock, and the DMA
//! channel feeding it. It produces every sample itself, a chunk at a time,
//! as the hardware asks for them. Nothing on core 0 has to keep it fed,
//! which matters because Doom's tics are uneven: a frame that runs long,
//! or a level load that stalls core 0 for a second, can't starve it.
//!
//! The samples come from the sound-effects mixer (src/sound.rs): core 0
//! only says which sounds are playing on which channels, and core 1 mixes
//! them here, a chunk at a time.
//!
//! # How the samples reach the pins
//!
//! PWM channels 1 and 2 drive GPIO40 and GPIO45, the pins wired to the
//! board's own audio filter, so the jack needs nothing external. They run
//! in FIFO mode with a DMA channel feeding the FIFO, paced by the PWM's own
//! DREQ: no core writes a register per sample. The FIFO hands alternate
//! words to the two channels, so a chunk is interleaved left, right.
//!
//! The DMA side is double-buffered ([`rpi_hal::dma::Channel::stream_peripheral`]):
//! the engine plays one chunk while core 1 fills the other, and filling
//! blocks until the engine is done with that chunk, so the loop is paced
//! by the audio hardware and needs no timer.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};

use rpi_hal::dma::Dma;
use rpi_hal::multicore::{CacheAligned, Core1, Stack};
use rpi_hal::pwm::{Channel1Pin, Channel2Pin, Pwm, pcm_to_duty};
use rpi_hal::{halt, pac};

use crate::sound::Mixer;

/// Duty range a sample is expressed in, and so the bit depth: 10 bits.
///
/// It trades against the sample rate, which is `pwm_clock / RANGE`. 1024
/// at a ~50 MHz PWM clock lands just under 49 kHz.
const RANGE: u16 = 1024;

/// Sample rate the PWM clock divisor is chosen for. The divisor is an
/// integer, so the real rate is the nearest it can express; see
/// [`sample_rate_hz`].
const TARGET_RATE_HZ: u32 = 48_000;

/// Nominal frequency of `PLLD_per`, the clock `rpi_hal::pwm` divides down.
const PWM_CLOCK_HZ: u32 = 500_000_000;

/// The divisor the PWM clock runs at.
const DIVISOR: u16 = Pwm::audio_clock_divisor(TARGET_RATE_HZ, RANGE);

/// The DMA channel that feeds the PWM FIFO. The VideoCore firmware is
/// still running underneath and owns some channels; 5 is among those it
/// leaves to the ARM.
const DMA_CHANNEL: u8 = 5;

/// Stereo frames per chunk. Two chunks are in flight at once, so this is
/// the latency: 2 x 256 frames is about 10 ms.
const CHUNK_FRAMES: usize = 256;

/// Words per chunk: one per channel per frame.
const CHUNK_WORDS: usize = CHUNK_FRAMES * 2;

/// Core 1's stack. The fill loop is flat and its buffers are `static`.
static mut CORE1_STACK: Stack<0x4000> = Stack::new();

/// One of the two chunk buffers the DMA engine alternates between, aligned
/// to a cache line so cleaning it for the engine touches nothing else.
#[repr(C, align(64))]
struct Chunk([u32; CHUNK_WORDS]);

// Handed to the DMA stream on core 1 and borrowed by it forever after.
static mut BUF0: Chunk = Chunk([0; CHUNK_WORDS]);
static mut BUF1: Chunk = Chunk([0; CHUNK_WORDS]);

/// Chunks core 1 has handed to the DMA engine: the liveness signal. Its
/// rate is fixed by the audio clock, one per [`CHUNK_FRAMES`] frames, so a
/// count that stops climbing means the stream has stopped. Written only by
/// core 1, with a plain store: an exclusive read-modify-write is a `wfe`
/// wake-up event for any core idling alongside.
static CHUNKS: CacheAligned<AtomicU32> = CacheAligned(AtomicU32::new(0));

/// The rate the PWM hardware clocks frames out at, in Hz.
pub fn sample_rate_hz() -> u32 {
    PWM_CLOCK_HZ / (u32::from(DIVISOR) * u32::from(RANGE))
}

/// Chunks core 1 has handed to the DMA engine since boot. Zero means that
/// core never got the hardware up.
pub fn chunks() -> u32 {
    CHUNKS.0.load(Ordering::Relaxed)
}

/// Starts audio output on core 1.
///
/// Call after the SD card is up: core 1 muxes GPIO40/45 through `GPFSEL4`,
/// which also holds GPIO48/49's function bits that the SD bring-up writes,
/// and a spawn orders everything before it against everything the spawned
/// core does.
pub fn start(core1: Core1) {
    // Safety: `worker` never returns and touches only hardware core 0
    // doesn't use (see its comments) and its own statics. The stack is
    // referenced only here.
    unsafe { core1.spawn(&mut *addr_of_mut!(CORE1_STACK), worker) };
}

/// Fills one chunk with [`CHUNK_FRAMES`] interleaved frames from the
/// mixer.
fn fill(mixer: &mut Mixer, buf: &mut [u32]) {
    let mut frames = [[0i16; 2]; CHUNK_FRAMES];
    mixer.mix(&mut frames);
    for (words, frame) in buf.as_chunks_mut::<2>().0.iter_mut().zip(&frames) {
        words[0] = u32::from(pcm_to_duty(frame[0], RANGE));
        words[1] = u32::from(pcm_to_duty(frame[1], RANGE));
    }
    CHUNKS.0.store(
        CHUNKS.0.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
}

/// The audio worker: owns the PWM and the DMA channel feeding it, and keeps
/// the channel supplied for as long as the machine runs.
///
/// Stays off `critical-section`: with rpi-hal's `multicore` feature that's
/// a lock shared with core 0, and the fill loop mustn't wait on whatever
/// core 0 is doing. That includes `logln!`.
extern "C" fn worker() -> ! {
    // A second view of the peripherals, for this core. It touches only the
    // PWM block, `CM_PWM` and the DMA controller, none of which core 0
    // uses, plus `GPFSEL4` (see `start`).
    let peripherals = unsafe { pac::Peripherals::steal() };

    let pwm = Pwm::init(peripherals.PWM0, peripherals.CM_PWM, DIVISOR);
    let audio = pwm.audio(
        &peripherals.GPIO,
        Channel1Pin::Gpio40,
        Channel2Pin::Gpio45,
        RANGE,
    );

    // Safety: only this core names these, and from the stream below on
    // they stay borrowed by it for the rest of the program.
    let buf0 = unsafe { &mut *addr_of_mut!(BUF0) };
    let buf1 = unsafe { &mut *addr_of_mut!(BUF1) };

    // Both chunks hold real audio before the engine starts: it reads the
    // moment the stream is armed.
    let mut mixer = Mixer::new();
    fill(&mut mixer, &mut buf0.0);
    fill(&mut mixer, &mut buf1.0);

    let mut dma = Dma::new();
    // Parking is the right failure for both of these: the game runs on
    // without sound, and the chunk count shows it stopped.
    let Some(mut channel) = dma.channel(DMA_CHANNEL) else {
        halt();
    };
    let Ok(mut stream) = channel.stream_peripheral(
        [&mut buf0.0, &mut buf1.0],
        audio.dreq(),
        audio.fifo_bus_address(),
    ) else {
        halt();
    };

    loop {
        // Blocks until the engine is done with the chunk being refilled.
        stream.feed(|buf| fill(&mut mixer, buf));
        // A channel that flagged an error has stopped, and a stopped
        // channel is what paces this loop, so carrying on would spin.
        if stream.is_error() {
            halt();
        }
    }
}

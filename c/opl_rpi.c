// This board's OPL library: the `OPL_*` functions Chocolate Doom's music
// module (chocolate-doom/i_oplmusic.c) is written against, on Nuked OPL3
// (src/opl3.rs) rendered on core 1.
//
// In place of Chocolate Doom's opl/opl.c and its SDL driver. The shape is
// the SDL driver's: the music module schedules callbacks ("in N us, play
// the next event"), and whoever renders the chip's samples advances a
// clock by the samples rendered and fires each callback when the clock
// reaches it. Here that is core 1, in rpi_doom_opl_render, a chunk at a
// time.
//
// One lock guards all of it, the chip, the callback queue and the music
// module's own state: core 0 holds it to change songs or volume
// (c/i_rpisound.c), core 1 holds it while it renders and fires callbacks.
// It is re-entrant, because the music module locks again inside calls
// that are already locked.
//
// What opl.c also does and this doesn't: driver selection, and chip
// detection through the OPL timers. There is one chip here and it's
// known to be there.

#include <stdint.h>
#include <stdlib.h>

#include "opl.h"
#include "opl_queue.h"

// The chip, in Rust (src/music.rs).
void rpi_doom_opl_lock(void);
void rpi_doom_opl_unlock(void);
void rpi_doom_opl_reset(void);
unsigned int rpi_doom_opl_rate(void);
void rpi_doom_opl_write(unsigned int reg, unsigned int value);
void rpi_doom_opl_generate(int16_t *out, unsigned int frames);

static opl_callback_queue_t *callback_queue;

// Microseconds of audio rendered since OPL_Init: the clock callbacks are
// scheduled against.
static uint64_t current_time;

// Whether callbacks are paused, and how long they have been in total, so
// a paused song resumes where it was rather than skipping ahead.
static int paused;
static uint64_t pause_offset;

opl_init_result_t OPL_Init(unsigned int port_base)
{
    rpi_doom_opl_lock();
    if (callback_queue == NULL)
    {
        // On core 0, at startup: the only allocation here.
        callback_queue = OPL_Queue_Create();
    }
    OPL_Queue_Clear(callback_queue);
    current_time = 0;
    paused = 0;
    pause_offset = 0;
    rpi_doom_opl_reset();
    rpi_doom_opl_unlock();
    // Nuked emulates an OPL3; the music module still runs it as an OPL2
    // unless DMXOPTION asks for -opl3.
    return OPL_INIT_OPL3;
}

void OPL_Shutdown(void)
{
    OPL_ClearCallbacks();
}

void OPL_SetSampleRate(unsigned int rate)
{
    // The chip renders at core 1's output rate, whatever the config says.
}

void OPL_WriteRegister(int reg, int value)
{
    switch (reg)
    {
        case OPL_REG_TIMER1:
        case OPL_REG_TIMER2:
        case OPL_REG_TIMER_CTRL:
            // The timers only matter for detection, which isn't done
            // here; the SDL driver doesn't pass them to the chip either.
            break;

        default:
            rpi_doom_opl_lock();
            rpi_doom_opl_write(reg, value);
            rpi_doom_opl_unlock();
            break;
    }
}

// As opl.c's, register for register: what Doom wrote at startup, including
// the registers that don't exist.
void OPL_InitRegisters(int opl3)
{
    int r;

    for (r = OPL_REGS_LEVEL; r <= OPL_REGS_LEVEL + OPL_NUM_OPERATORS; ++r)
    {
        OPL_WriteRegister(r, 0x3f);
    }
    for (r = OPL_REGS_ATTACK; r <= OPL_REGS_WAVEFORM + OPL_NUM_OPERATORS; ++r)
    {
        OPL_WriteRegister(r, 0x00);
    }
    for (r = 1; r < OPL_REGS_LEVEL; ++r)
    {
        OPL_WriteRegister(r, 0x00);
    }
    OPL_WriteRegister(OPL_REG_TIMER_CTRL, 0x60);
    OPL_WriteRegister(OPL_REG_TIMER_CTRL, 0x80);
    OPL_WriteRegister(OPL_REG_WAVEFORM_ENABLE, 0x20);

    if (opl3)
    {
        OPL_WriteRegister(OPL_REG_NEW, 0x01);
        for (r = OPL_REGS_LEVEL; r <= OPL_REGS_LEVEL + OPL_NUM_OPERATORS; ++r)
        {
            OPL_WriteRegister(r | 0x100, 0x3f);
        }
        for (r = OPL_REGS_ATTACK; r <= OPL_REGS_WAVEFORM + OPL_NUM_OPERATORS; ++r)
        {
            OPL_WriteRegister(r | 0x100, 0x00);
        }
        for (r = 1; r < OPL_REGS_LEVEL; ++r)
        {
            OPL_WriteRegister(r | 0x100, 0x00);
        }
    }

    // Keyboard split point on (opl.c's "?" is its own).
    OPL_WriteRegister(OPL_REG_FM_MODE, 0x40);

    if (opl3)
    {
        OPL_WriteRegister(OPL_REG_NEW, 0x01);
    }
}

void OPL_SetCallback(uint64_t us, opl_callback_t callback, void *data)
{
    rpi_doom_opl_lock();
    OPL_Queue_Push(callback_queue, callback, data, current_time - pause_offset + us);
    rpi_doom_opl_unlock();
}

void OPL_ClearCallbacks(void)
{
    rpi_doom_opl_lock();
    if (callback_queue != NULL)
    {
        OPL_Queue_Clear(callback_queue);
    }
    rpi_doom_opl_unlock();
}

void OPL_AdjustCallbacks(float factor)
{
    rpi_doom_opl_lock();
    OPL_Queue_AdjustCallbacks(callback_queue, current_time, factor);
    rpi_doom_opl_unlock();
}

void OPL_Lock(void)
{
    rpi_doom_opl_lock();
}

void OPL_Unlock(void)
{
    rpi_doom_opl_unlock();
}

void OPL_SetPaused(int _paused)
{
    rpi_doom_opl_lock();
    paused = _paused;
    rpi_doom_opl_unlock();
}

// Advances the clock by `frames`, firing every callback that comes due.
// A callback may schedule more (the next event of its track), which this
// fires too if they are already due.
static void AdvanceTime(unsigned int frames)
{
    opl_callback_t callback;
    void *data;
    uint64_t us = ((uint64_t) frames * OPL_SECOND) / rpi_doom_opl_rate();

    current_time += us;
    if (paused)
    {
        pause_offset += us;
    }
    while (!OPL_Queue_IsEmpty(callback_queue)
        && current_time >= OPL_Queue_Peek(callback_queue) + pause_offset)
    {
        if (!OPL_Queue_Pop(callback_queue, &callback, &data))
        {
            break;
        }
        callback(data);
    }
}

// Renders `frames` stereo frames of music into `out` (interleaved left,
// right). Called on core 1, a chunk at a time. Silence until OPL_Init.
//
// Renders in slices that end where the next callback is due, so each
// event lands on its sample, as the SDL driver does.
void rpi_doom_opl_render(int16_t *out, unsigned int frames)
{
    unsigned int filled = 0;
    unsigned int rate;

    rpi_doom_opl_lock();
    if (callback_queue == NULL)
    {
        rpi_doom_opl_unlock();
        for (filled = 0; filled < frames * 2; ++filled)
        {
            out[filled] = 0;
        }
        return;
    }

    rate = rpi_doom_opl_rate();
    while (filled < frames)
    {
        uint64_t slice = frames - filled;

        if (!paused && !OPL_Queue_IsEmpty(callback_queue))
        {
            uint64_t due = OPL_Queue_Peek(callback_queue) + pause_offset;
            uint64_t until = due > current_time ? due - current_time : 0;
            uint64_t n = (until * rate + OPL_SECOND - 1) / OPL_SECOND;

            if (n < slice)
            {
                slice = n;
            }
        }

        rpi_doom_opl_generate(out + filled * 2, (unsigned int) slice);
        filled += (unsigned int) slice;
        AdvanceTime((unsigned int) slice);
    }
    rpi_doom_opl_unlock();
}

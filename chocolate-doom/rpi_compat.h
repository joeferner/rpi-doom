// rpi-doom: what these files need from Chocolate Doom's own headers that
// doomgeneric's versions of them don't have. Included by i_oplmusic.c and
// midifile.c after their own includes.

#ifndef RPI_COMPAT_H
#define RPI_COMPAT_H

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#include "doomtype.h"
#include "i_system.h"

// From Chocolate Doom's doomtype.h: doomgeneric's has PACKEDATTR, but not
// this. The MSVC prefix half of upstream's definition is empty for gcc.
#define PACKED_STRUCT(...) struct __VA_ARGS__ PACKEDATTR

// Chocolate Doom's i_swap.h takes these from SDL. MIDI is big-endian and
// this core is little-endian.
#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error "rpi_compat.h assumes a little-endian target"
#endif
#define SDL_SwapBE16(x) __builtin_bswap16((uint16_t) (x))
#define SDL_SwapBE32(x) __builtin_bswap32((uint32_t) (x))

// From Chocolate Doom's i_system.c: realloc that treats running out of
// memory as fatal.
static inline void *I_Realloc(void *ptr, size_t size)
{
    void *new_ptr = realloc(ptr, size);

    if (size != 0 && new_ptr == NULL)
    {
        I_Error("I_Realloc: failed on reallocation of %u bytes", (unsigned int) size);
    }
    return new_ptr;
}

#endif

// The engine's sound and music modules for this board.
//
// The sound module finds each effect's lump and hands its samples to the
// mixer on core 1 (src/sound.rs). The samples are played straight out of
// the lump cache: the engine keeps the lump at PU_STATIC, so the memory
// never moves or goes away while a channel is reading it, and nothing is
// converted or copied.
//
// The music module does nothing yet (#8).

#include <stddef.h>
#include <stdint.h>

#include "deh_str.h"
#include "doomtype.h"
#include "i_sound.h"
#include "m_misc.h"
#include "w_wad.h"
#include "z_zone.h"

// The mixer, in Rust (src/sound.rs). `left` and `right` are gains,
// 0-255.
void rpi_doom_sound_start(int channel, const uint8_t *samples, uint32_t len,
                          uint32_t rate, int left, int right);
void rpi_doom_sound_params(int channel, int left, int right);
void rpi_doom_sound_stop(int channel);
int rpi_doom_sound_playing(int channel);
int rpi_doom_sound_channels(void);

// Bound to the config by i_sound.c when FEATURE_SOUND is on; the SDL
// module defines them. Nothing here resamples through libsamplerate.
int use_libsamplerate = 0;
float libsamplerate_scale = 0.65f;

static boolean use_sfx_prefix;

static snddevice_t sound_devices[] = {
    SNDDEVICE_SB,          SNDDEVICE_PAS,         SNDDEVICE_GUS,
    SNDDEVICE_WAVEBLASTER, SNDDEVICE_SOUNDCANVAS, SNDDEVICE_AWE32,
};

static boolean Init(boolean _use_sfx_prefix)
{
    use_sfx_prefix = _use_sfx_prefix;
    // Core 1 is already playing (src/audio.rs).
    return true;
}

static void Shutdown(void)
{
}

static int GetSfxLumpNum(sfxinfo_t *sfx)
{
    char name[9];

    // A linked sound plays the lump of the one it links to.
    if (sfx->link != NULL)
    {
        sfx = sfx->link;
    }
    // Doom's sound lumps are the effect's name with a DS prefix.
    if (use_sfx_prefix)
    {
        M_snprintf(name, sizeof(name), "ds%s", DEH_String(sfx->name));
    }
    else
    {
        M_StringCopy(name, DEH_String(sfx->name), sizeof(name));
    }
    return W_GetNumForName(name);
}

static void Update(void)
{
}

// The stereo gains for a volume (0-127) and separation (0-254, 128 the
// centre), as the SDL module and the DOS mixer it imitates work them out:
// fully off to one side is twice the centre's gain in that ear and none
// in the other.
static void Gains(int vol, int sep, int *left, int *right)
{
    *left = ((254 - sep) * vol) / 127;
    *right = (sep * vol) / 127;
    if (*left < 0) *left = 0;
    if (*left > 255) *left = 255;
    if (*right < 0) *right = 0;
    if (*right > 255) *right = 255;
}

static void UpdateSoundParams(int channel, int vol, int sep)
{
    int left, right;

    if (channel < 0 || channel >= rpi_doom_sound_channels())
    {
        return;
    }
    Gains(vol, sep, &left, &right);
    rpi_doom_sound_params(channel, left, right);
}

static int StartSound(sfxinfo_t *sfx, int channel, int vol, int sep)
{
    const uint8_t *data;
    unsigned int lumplen;
    unsigned int rate;
    unsigned int length;
    int left, right;

    if (channel < 0 || channel >= rpi_doom_sound_channels())
    {
        return -1;
    }

    // A DMX sound lump: format 3 (16 bits), sample rate (16 bits), sample
    // count (32 bits), then unsigned 8-bit mono samples.
    data = W_CacheLumpNum(sfx->lumpnum, PU_STATIC);
    lumplen = W_LumpLength(sfx->lumpnum);
    if (lumplen < 8 || data[0] != 0x03 || data[1] != 0x00)
    {
        return -1;
    }
    rate = (data[3] << 8) | data[2];
    length = ((unsigned int)data[7] << 24) | (data[6] << 16) | (data[5] << 8) | data[4];

    // Discarded as too short or longer than the lump, as DMX does.
    if (length > lumplen - 8 || length <= 48)
    {
        return -1;
    }
    // DMX skips the first and last 16 samples, which are padding.
    data += 8 + 16;
    length -= 32;

    Gains(vol, sep, &left, &right);
    rpi_doom_sound_start(channel, data, length, rate, left, right);
    return channel;
}

static void StopSound(int channel)
{
    if (channel < 0 || channel >= rpi_doom_sound_channels())
    {
        return;
    }
    rpi_doom_sound_stop(channel);
}

static boolean SoundIsPlaying(int channel)
{
    if (channel < 0 || channel >= rpi_doom_sound_channels())
    {
        return false;
    }
    return rpi_doom_sound_playing(channel) != 0;
}

static void CacheSounds(sfxinfo_t *sounds, int num_sounds)
{
    // Lumps are loaded the first time each sound plays.
}

sound_module_t DG_sound_module = {
    sound_devices,
    arrlen(sound_devices),
    Init,
    Shutdown,
    GetSfxLumpNum,
    Update,
    UpdateSoundParams,
    StartSound,
    StopSound,
    SoundIsPlaying,
    CacheSounds,
};

// Music: nothing yet (#8). Init succeeds so the engine carries on, and
// every song is "registered" and plays silently.

static boolean MusicInit(void)
{
    return true;
}

static void MusicShutdown(void)
{
}

static void SetMusicVolume(int volume)
{
}

static void PauseMusic(void)
{
}

static void ResumeMusic(void)
{
}

static void *RegisterSong(void *data, int len)
{
    // Non-null: a null handle reads as "couldn't register".
    return data;
}

static void UnRegisterSong(void *handle)
{
}

static void PlaySong(void *handle, boolean looping)
{
}

static void StopSong(void)
{
}

static boolean MusicIsPlaying(void)
{
    return false;
}

static void Poll(void)
{
}

music_module_t DG_music_module = {
    sound_devices,
    arrlen(sound_devices),
    MusicInit,
    MusicShutdown,
    SetMusicVolume,
    PauseMusic,
    ResumeMusic,
    RegisterSong,
    UnRegisterSong,
    PlaySong,
    StopSong,
    MusicIsPlaying,
    Poll,
};

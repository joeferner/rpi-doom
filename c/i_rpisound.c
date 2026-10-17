// The engine's sound and music modules for this board.
//
// The sound module finds each effect's lump and hands its samples to the
// mixer on core 1 (src/sound.rs). The samples are played straight out of
// the lump cache: the engine keeps the lump at PU_STATIC, so the memory
// never moves or goes away while a channel is reading it, and nothing is
// converted or copied.
//
// The music module is Chocolate Doom's OPL one, behind a lock (see below).

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

// Music: Chocolate Doom's OPL music module (chocolate-doom/i_oplmusic.c),
// with every call that touches playback state taken under the OPL lock.
//
// The module's callbacks run on core 1 and walk the same voices and
// tracks that PlaySong, StopSong, the volume and pausing change on core 0.
// Upstream locks only inside StopSong; the lock is re-entrant, so taking
// it again around each call is safe and covers the rest.
//
// Two calls stay unlocked. Init loads the GENMIDI lump from the card, which
// would hold core 1 off for the length of a read, and nothing is playing
// yet. RegisterSong only parses a song into memory core 1 never sees
// until PlaySong.

// music_opl_module is declared in i_sound.h.
void OPL_Lock(void);
void OPL_Unlock(void);

static boolean MusicInit(void)
{
    return music_opl_module.Init();
}

static void MusicShutdown(void)
{
    OPL_Lock();
    music_opl_module.Shutdown();
    OPL_Unlock();
}

static void SetMusicVolume(int volume)
{
    OPL_Lock();
    music_opl_module.SetMusicVolume(volume);
    OPL_Unlock();
}

static void PauseMusic(void)
{
    OPL_Lock();
    music_opl_module.PauseMusic();
    OPL_Unlock();
}

static void ResumeMusic(void)
{
    OPL_Lock();
    music_opl_module.ResumeMusic();
    OPL_Unlock();
}

static void *RegisterSong(void *data, int len)
{
    return music_opl_module.RegisterSong(data, len);
}

static void UnRegisterSong(void *handle)
{
    OPL_Lock();
    music_opl_module.UnRegisterSong(handle);
    OPL_Unlock();
}

static void PlaySong(void *handle, boolean looping)
{
    OPL_Lock();
    music_opl_module.PlaySong(handle, looping);
    OPL_Unlock();
}

static void StopSong(void)
{
    OPL_Lock();
    music_opl_module.StopSong();
    OPL_Unlock();
}

static boolean MusicIsPlaying(void)
{
    boolean playing;

    OPL_Lock();
    playing = music_opl_module.MusicIsPlaying();
    OPL_Unlock();
    return playing;
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
    // The OPL module has nothing to poll: its callbacks run on core 1.
    NULL,
};

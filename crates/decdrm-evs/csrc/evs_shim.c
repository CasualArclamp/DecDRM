/*
 * Frame-by-frame interface to the 3GPP EVS floating-point reference decoder
 * (TS 26.443) for DecDRM. It replaces the command-line loop of lib_dec/decoder.c:
 * frames come from memory through read_indices_from_djb() (the reference code's
 * de-jitter-buffer reader, as lib_dec/EvsRXlib.c uses it) instead of a G.192 file.
 * Part of DecDRM (GPL-2.0-or-later); the reference code it calls is not.
 */

#include <stdlib.h>
#include <string.h>
#include "options.h"
#include "prot.h"
#include "cnst.h"
#include "rom_com.h"

/* Frame counter that lib_dec/decoder.c (not compiled) defines for the VoIP client. */
long frame = 0;

typedef struct
{
    Decoder_State *st;
    int started;        /* init_decoder() ran (it needs the first frame's bit rate) */
    short output_frame; /* samples per 20 ms at the output rate */
} DecdrmEvs;

/* A decoder writing `output_fs` Hz (8000, 16000, 32000 or 48000); NULL on failure. */
void *decdrm_evs_open( int output_fs )
{
    DecdrmEvs *d;
    Decoder_State *st;

    if( output_fs != 8000 && output_fs != 16000 && output_fs != 32000 && output_fs != 48000 )
    {
        return NULL;
    }
    d = (DecdrmEvs *) calloc( 1, sizeof( DecdrmEvs ) );
    if( d == NULL )
    {
        return NULL;
    }
    st = (Decoder_State *) calloc( 1, sizeof( Decoder_State ) );
    if( st == NULL )
    {
        free( d );
        return NULL;
    }
    /* What decoder.c's main() and io_ini_dec() set before init_decoder(). */
    st->cldfbAna = st->cldfbBPF = st->cldfbSyn = NULL;
    st->hFdCngDec = NULL;
    st->writeFECoffset = 0;
    st->codec_mode = 0; /* unknown before the first frame */
    st->Opt_AMR_WB = 0;
    st->Opt_VOIP = 0;
    st->bitstreamformat = G192;
    st->amrwb_rfc4867_flag = -1;
    st->output_Fs = output_fs;
    d->st = st;
    d->output_frame = (short) ( output_fs / 50 );
    return d;
}

/*
 * Decode one frame: `nbits` bits packed MSB first in `bits` (a 13.2 kbit/s frame is
 * 264 bits), or conceal a lost frame (bits == NULL or nbits == 0). Writes 20 ms of
 * mono samples (16-bit scale, as float) to `out`, which holds 960 samples. Returns the
 * number of samples written, or -1 for an invalid frame size.
 */
int decdrm_evs_decode( void *handle, const unsigned char *bits, int nbits, float *out )
{
    DecdrmEvs *d = (DecdrmEvs *) handle;
    Decoder_State *st = d->st;
    float output[L_FRAME48k];
    unsigned char packed[MAX_BITS_PER_FRAME / 8 + 1];

    if( nbits < 0 || nbits > MAX_BITS_PER_FRAME )
    {
        return -1;
    }
    if( bits == NULL || nbits == 0 )
    {
        if( !d->started )
        {
            /* Nothing to conceal from yet. */
            memset( out, 0, sizeof( float ) * d->output_frame );
            return d->output_frame;
        }
        read_indices_from_djb( st, NULL, 0, 0, 0, 0, 0, 0 );
    }
    else
    {
        /* read_indices_from_djb() takes a non-const pointer. */
        memcpy( packed, bits, (size_t) ( nbits + 7 ) / 8 );
        if( !d->started )
        {
            /* As lib_dec/EvsRXlib.c with the first frame: init_decoder() sets its own
               defaults (it needs only output_Fs), then the frame is parsed. */
            st->ini_frame = 0;
            st->prev_use_partial_copy = 0;
            init_decoder( st );
            d->started = 1;
        }
        read_indices_from_djb( st, packed, nbits, 0, 0, 1, 0, 0 );
    }

    if( st->codec_mode == MODE1 )
    {
        evs_dec( st, output, FRAMEMODE_NORMAL );
    }
    else if( st->bfi == 0 )
    {
        evs_dec( st, output, FRAMEMODE_NORMAL );
    }
    else if( st->bfi == 2 )
    {
        evs_dec( st, output, FRAMEMODE_FUTURE );
    }
    else
    {
        evs_dec( st, output, FRAMEMODE_MISSING );
    }
    if( st->ini_frame < MAX_FRAME_COUNTER )
    {
        st->ini_frame++;
    }
    memcpy( out, output, sizeof( float ) * d->output_frame );
    return d->output_frame;
}

void decdrm_evs_close( void *handle )
{
    DecdrmEvs *d = (DecdrmEvs *) handle;

    if( d == NULL )
    {
        return;
    }
    if( d->started )
    {
        destroy_decoder( d->st );
    }
    free( d->st );
    free( d );
}

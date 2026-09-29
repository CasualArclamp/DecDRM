/* DecDRM helper shim compiled together with the libxaac encoder.
 *
 * 1. Exports sizeof/offsetof of every public struct that decdrm-xaac-sys
 *    transcribes by hand, so Rust unit tests can check the hand-written
 *    #[repr(C)] layouts against the real compiler's view (there is no bindgen
 *    on the build machines).
 * 2. Provides the aligned allocator pair that libxaac expects in
 *    ixheaace_output_config (malloc_xheaace / free_xheaace), equivalent to the
 *    one in libxaac's test bench.
 * 3. Reports sizeof(ia_drc_input_config): ixheaace_create() writes a whole
 *    ia_drc_input_config through ixheaace_input_config.pv_drc_cfg, so the
 *    caller must provide a buffer of at least this size.
 *
 * This file is part of DecDRM (GPL-2.0-or-later); it only includes libxaac's
 * public headers.
 */

#include <stddef.h>
#include <stdlib.h>
#if defined(_WIN32)
#include <malloc.h>
#endif

#include "ixheaac_type_def.h"
#include "impd_drc_common_enc.h"
#include "impd_drc_uni_drc.h"
#include "impd_drc_tables.h"
#include "impd_drc_api.h"
#include "ixheaace_api.h"

typedef struct {
  const char *name;
  size_t value;
} DecdrmLayoutEntry;

#define DECDRM_SIZE(T) {"sizeof " #T, sizeof(T)}
#define DECDRM_OFF(T, f) {#T "." #f, offsetof(T, f)}

static const DecdrmLayoutEntry kLayout[] = {
    /* scalar typedefs */
    DECDRM_SIZE(WORD32),
    DECDRM_SIZE(UWORD32),
    DECDRM_SIZE(FLAG),
    DECDRM_SIZE(FLOAT32),
    DECDRM_SIZE(FLOAT64),
    DECDRM_SIZE(UWORD16),
    DECDRM_SIZE(SIZE_T),
    DECDRM_SIZE(pVOID),

    /* ixheaace_mem_info_table */
    DECDRM_SIZE(ixheaace_mem_info_table),
    DECDRM_OFF(ixheaace_mem_info_table, ui_size),
    DECDRM_OFF(ixheaace_mem_info_table, ui_alignment),
    DECDRM_OFF(ixheaace_mem_info_table, ui_type),
    DECDRM_OFF(ixheaace_mem_info_table, mem_ptr),

    /* ixheaace_aac_enc_config */
    DECDRM_SIZE(ixheaace_aac_enc_config),
    DECDRM_OFF(ixheaace_aac_enc_config, sample_rate),
    DECDRM_OFF(ixheaace_aac_enc_config, bitrate),
    DECDRM_OFF(ixheaace_aac_enc_config, num_channels_in),
    DECDRM_OFF(ixheaace_aac_enc_config, num_channels_out),
    DECDRM_OFF(ixheaace_aac_enc_config, bandwidth),
    DECDRM_OFF(ixheaace_aac_enc_config, dual_mono),
    DECDRM_OFF(ixheaace_aac_enc_config, use_tns),
    DECDRM_OFF(ixheaace_aac_enc_config, noise_filling),
    DECDRM_OFF(ixheaace_aac_enc_config, use_adts),
    DECDRM_OFF(ixheaace_aac_enc_config, private_bit),
    DECDRM_OFF(ixheaace_aac_enc_config, copyright_bit),
    DECDRM_OFF(ixheaace_aac_enc_config, original_copy_bit),
    DECDRM_OFF(ixheaace_aac_enc_config, f_no_stereo_preprocessing),
    DECDRM_OFF(ixheaace_aac_enc_config, inv_quant),
    DECDRM_OFF(ixheaace_aac_enc_config, full_bandwidth),
    DECDRM_OFF(ixheaace_aac_enc_config, bitreservoir_size),
    DECDRM_OFF(ixheaace_aac_enc_config, length),

    /* ixheaace_input_config */
    DECDRM_SIZE(ixheaace_input_config),
    DECDRM_OFF(ixheaace_input_config, ui_pcm_wd_sz),
    DECDRM_OFF(ixheaace_input_config, i_bitrate),
    DECDRM_OFF(ixheaace_input_config, frame_length),
    DECDRM_OFF(ixheaace_input_config, frame_cmd_flag),
    DECDRM_OFF(ixheaace_input_config, out_bytes_flag),
    DECDRM_OFF(ixheaace_input_config, user_tns_flag),
    DECDRM_OFF(ixheaace_input_config, user_esbr_flag),
    DECDRM_OFF(ixheaace_input_config, aot),
    DECDRM_OFF(ixheaace_input_config, i_mps_tree_config),
    DECDRM_OFF(ixheaace_input_config, esbr_flag),
    DECDRM_OFF(ixheaace_input_config, i_channels),
    DECDRM_OFF(ixheaace_input_config, i_samp_freq),
    DECDRM_OFF(ixheaace_input_config, i_native_samp_freq),
    DECDRM_OFF(ixheaace_input_config, i_channels_mask),
    DECDRM_OFF(ixheaace_input_config, i_num_coupling_chan),
    DECDRM_OFF(ixheaace_input_config, i_use_mps),
    DECDRM_OFF(ixheaace_input_config, i_use_adts),
    DECDRM_OFF(ixheaace_input_config, i_use_es),
    DECDRM_OFF(ixheaace_input_config, usac_en),
    DECDRM_OFF(ixheaace_input_config, codec_mode),
    DECDRM_OFF(ixheaace_input_config, cplx_pred),
    DECDRM_OFF(ixheaace_input_config, ccfl_idx),
    DECDRM_OFF(ixheaace_input_config, pvc_active),
    DECDRM_OFF(ixheaace_input_config, harmonic_sbr),
    DECDRM_OFF(ixheaace_input_config, inter_tes_active),
    DECDRM_OFF(ixheaace_input_config, pv_drc_cfg),
    DECDRM_OFF(ixheaace_input_config, use_drc_element),
    DECDRM_OFF(ixheaace_input_config, drc_frame_size),
    DECDRM_OFF(ixheaace_input_config, hq_esbr),
    DECDRM_OFF(ixheaace_input_config, write_program_config_element),
    DECDRM_OFF(ixheaace_input_config, aac_config),
    DECDRM_OFF(ixheaace_input_config, random_access_interval),
    DECDRM_OFF(ixheaace_input_config, method_def),
    DECDRM_OFF(ixheaace_input_config, measured_loudness),
    DECDRM_OFF(ixheaace_input_config, measurement_system),
    DECDRM_OFF(ixheaace_input_config, sample_peak_level),
    DECDRM_OFF(ixheaace_input_config, stream_id),
    DECDRM_OFF(ixheaace_input_config, use_delay_adjustment),

    /* ixheaace_version */
    DECDRM_SIZE(ixheaace_version),
    DECDRM_OFF(ixheaace_version, p_lib_name),
    DECDRM_OFF(ixheaace_version, p_version_num),

    /* ixheaace_output_config */
    DECDRM_SIZE(ixheaace_output_config),
    DECDRM_OFF(ixheaace_output_config, i_out_bytes),
    DECDRM_OFF(ixheaace_output_config, i_bytes_consumed),
    DECDRM_OFF(ixheaace_output_config, ui_inp_buf_size),
    DECDRM_OFF(ixheaace_output_config, malloc_count),
    DECDRM_OFF(ixheaace_output_config, ui_rem),
    DECDRM_OFF(ixheaace_output_config, ui_proc_mem_tabs_size),
    DECDRM_OFF(ixheaace_output_config, pv_ia_process_api_obj),
    DECDRM_OFF(ixheaace_output_config, arr_alloc_memory),
    DECDRM_OFF(ixheaace_output_config, malloc_xheaace),
    DECDRM_OFF(ixheaace_output_config, free_xheaace),
    DECDRM_OFF(ixheaace_output_config, version),
    DECDRM_OFF(ixheaace_output_config, mem_info_table),
    DECDRM_OFF(ixheaace_output_config, input_size),
    DECDRM_OFF(ixheaace_output_config, samp_freq),
    DECDRM_OFF(ixheaace_output_config, header_samp_freq),
    DECDRM_OFF(ixheaace_output_config, audio_profile),
    DECDRM_OFF(ixheaace_output_config, down_sampling_ratio),
    DECDRM_OFF(ixheaace_output_config, expected_frame_count),
    DECDRM_OFF(ixheaace_output_config, is_loudness_configured),
};

/* Number of entries in the layout table. */
size_t decdrm_xaac_layout_count(void) {
  return sizeof(kLayout) / sizeof(kLayout[0]);
}

/* Name ("sizeof X" or "Struct.field") and value of entry `i`.
 * Returns NULL (and leaves *value untouched) when `i` is out of range. */
const char *decdrm_xaac_layout_entry(size_t i, size_t *value) {
  if (i >= decdrm_xaac_layout_count()) return NULL;
  if (value != NULL) *value = kLayout[i].value;
  return kLayout[i].name;
}

/* Size in bytes of the ia_drc_input_config that pv_drc_cfg must point to. */
size_t decdrm_xaac_drc_config_size(void) { return sizeof(ia_drc_input_config); }

/* Aligned allocation for ixheaace_output_config.malloc_xheaace (the library asks
 * for 8-byte alignment). Returns NULL on failure. */
pVOID decdrm_xaac_malloc(UWORD32 size, UWORD32 alignment) {
  size_t align = alignment;
  if (align < sizeof(void *)) align = sizeof(void *);
  /* posix_memalign/_aligned_malloc need a power of two. */
  if ((align & (align - 1)) != 0) return NULL;
  if (size == 0) size = 1;
#if defined(_WIN32)
  return _aligned_malloc(size, align);
#else
  {
    void *ptr = NULL;
    if (posix_memalign(&ptr, align, size) != 0) return NULL;
    return ptr;
  }
#endif
}

/* Matching release function for ixheaace_output_config.free_xheaace. */
VOID decdrm_xaac_free(pVOID ptr) {
  if (ptr == NULL) return;
#if defined(_WIN32)
  _aligned_free(ptr);
#else
  free(ptr);
#endif
}

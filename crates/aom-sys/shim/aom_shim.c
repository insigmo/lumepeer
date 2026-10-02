// libaom realtime encoder wrapper (ADR 0139).
//
// This is the shim of the stage-1 measurement (tools/codec-bench/shim/
// aom_shim.c on the research/software-av1 branch, docs/research/
// software-av1.md), so the product encodes with exactly the configuration
// that was measured. Settings follow WebRTC's libaom_av1_encoder.cc (the RTC
// configuration Chrome ships for camera and screen sharing): one pass, no
// lag, CBR with a 600/600/1000 ms buffer, cyclic-refresh AQ, keyframes only
// when asked for.
//
// What changed against the bench, and nothing else: the configuration is kept
// so the bitrate can move without a new encoder (`aom_shim_set_bitrate`,
// `aom_codec_enc_config_set`), and errors and refused controls are kept for
// the caller to log instead of going to stderr.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "aom/aom_encoder.h"
#include "aom/aomcx.h"

typedef struct aom_shim {
  aom_codec_ctx_t ctx;
  aom_codec_enc_cfg_t cfg;
  int w, h;
  uint8_t *out;
  size_t out_cap;
  // Controls this build of libaom refused, comma-separated; empty when none.
  char refused[256];
  // The last error, for the caller to log.
  char error[256];
} aom_shim;

static int append(aom_shim *s, const void *data, size_t len, size_t *used) {
  if (*used + len > s->out_cap) {
    size_t cap = (*used + len) * 2;
    uint8_t *grown = (uint8_t *)realloc(s->out, cap);
    if (!grown) return -1;
    s->out = grown;
    s->out_cap = cap;
  }
  memcpy(s->out + *used, data, len);
  *used += len;
  return 0;
}

static void remember_error(aom_shim *s, const char *what) {
  const char *detail = aom_codec_error_detail(&s->ctx);
  snprintf(s->error, sizeof s->error, "%s: %s%s%s", what, aom_codec_error(&s->ctx),
           detail ? ": " : "", detail ? detail : "");
}

// A control the realtime-only build compiled out is not fatal: it is recorded,
// so the caller's log shows which tools were really in play.
static void ctrl(aom_shim *s, int id, int value, const char *name) {
  if (aom_codec_control(&s->ctx, id, value) != AOM_CODEC_OK) {
    size_t used = strlen(s->refused);
    snprintf(s->refused + used, sizeof s->refused - used, "%s%s=%d", used ? ", " : "", name, value);
  }
}
#define CTRL(id, v) ctrl(s, id, v, #id)

aom_shim *aom_shim_open(int w, int h, int fps, int kbps, int speed, int screen, int threads,
                        int tile_cols_log2, int min_q, int max_q, char *err, int errlen) {
  aom_codec_iface_t *iface = aom_codec_av1_cx();
  aom_codec_enc_cfg_t cfg;
  if (aom_codec_enc_config_default(iface, &cfg, AOM_USAGE_REALTIME) != AOM_CODEC_OK) {
    snprintf(err, errlen, "aom_codec_enc_config_default failed");
    return NULL;
  }
  cfg.g_w = (unsigned)w;
  cfg.g_h = (unsigned)h;
  cfg.g_timebase.num = 1;
  cfg.g_timebase.den = fps;
  cfg.g_threads = (unsigned)threads;
  cfg.g_lag_in_frames = 0;
  cfg.g_error_resilient = 0;
  cfg.g_pass = AOM_RC_ONE_PASS;
  cfg.rc_end_usage = AOM_CBR;
  cfg.rc_target_bitrate = (unsigned)kbps;
  cfg.rc_min_quantizer = (unsigned)min_q;
  cfg.rc_max_quantizer = (unsigned)max_q;
  cfg.rc_undershoot_pct = 50;
  cfg.rc_overshoot_pct = 50;
  cfg.rc_buf_initial_sz = 600;
  cfg.rc_buf_optimal_sz = 600;
  cfg.rc_buf_sz = 1000;
  cfg.rc_dropframe_thresh = 0;
  cfg.kf_mode = AOM_KF_DISABLED;

  aom_shim *s = (aom_shim *)calloc(1, sizeof(aom_shim));
  if (!s) {
    snprintf(err, errlen, "out of memory");
    return NULL;
  }
  s->w = w;
  s->h = h;
  if (aom_codec_enc_init(&s->ctx, iface, &cfg, 0) != AOM_CODEC_OK) {
    snprintf(err, errlen, "aom_codec_enc_init failed: %s", aom_codec_error(&s->ctx));
    free(s);
    return NULL;
  }
  s->cfg = cfg;
  CTRL(AOME_SET_CPUUSED, speed);
  CTRL(AV1E_SET_ROW_MT, 1);
  CTRL(AV1E_SET_TILE_COLUMNS, tile_cols_log2);
  CTRL(AV1E_SET_AQ_MODE, 3);
  CTRL(AV1E_SET_ENABLE_CDEF, 1);
  CTRL(AV1E_SET_ENABLE_TPL_MODEL, 0);
  CTRL(AV1E_SET_DELTAQ_MODE, 0);
  CTRL(AV1E_SET_ENABLE_ORDER_HINT, 0);
  CTRL(AV1E_SET_ENABLE_OBMC, 0);
  CTRL(AV1E_SET_ENABLE_WARPED_MOTION, 0);
  CTRL(AV1E_SET_ENABLE_GLOBAL_MOTION, 0);
  CTRL(AV1E_SET_ENABLE_REF_FRAME_MVS, 0);
  CTRL(AV1E_SET_NOISE_SENSITIVITY, 0);
  CTRL(AOME_SET_MAX_INTRA_BITRATE_PCT, 300);
  CTRL(AV1E_SET_COEFF_COST_UPD_FREQ, 3);
  CTRL(AV1E_SET_MODE_COST_UPD_FREQ, 3);
  CTRL(AV1E_SET_MV_COST_UPD_FREQ, 3);
  CTRL(AV1E_SET_TUNE_CONTENT, screen ? AOM_CONTENT_SCREEN : AOM_CONTENT_DEFAULT);
  CTRL(AV1E_SET_ENABLE_PALETTE, screen ? 1 : 0);
  return s;
}

// Encodes one I420 picture and returns the whole temporal unit it produced.
// Returns 0 on success, -1 on failure; *out_len is 0 if the encoder dropped it.
int aom_shim_encode(aom_shim *s, const uint8_t *y, const uint8_t *u, const uint8_t *v, int y_stride,
                    int uv_stride, int64_t pts, int force_kf, const uint8_t **out, size_t *out_len,
                    int *is_key) {
  aom_image_t img;
  if (!aom_img_wrap(&img, AOM_IMG_FMT_I420, (unsigned)s->w, (unsigned)s->h, 1, (unsigned char *)y)) {
    snprintf(s->error, sizeof s->error, "aom_img_wrap refused a %dx%d picture", s->w, s->h);
    return -1;
  }
  img.planes[AOM_PLANE_Y] = (unsigned char *)y;
  img.planes[AOM_PLANE_U] = (unsigned char *)u;
  img.planes[AOM_PLANE_V] = (unsigned char *)v;
  img.stride[AOM_PLANE_Y] = y_stride;
  img.stride[AOM_PLANE_U] = uv_stride;
  img.stride[AOM_PLANE_V] = uv_stride;

  if (aom_codec_encode(&s->ctx, &img, pts, 1, force_kf ? AOM_EFLAG_FORCE_KF : 0) != AOM_CODEC_OK) {
    remember_error(s, "aom_codec_encode");
    return -1;
  }
  size_t used = 0;
  int key = 0;
  aom_codec_iter_t iter = NULL;
  const aom_codec_cx_pkt_t *pkt;
  while ((pkt = aom_codec_get_cx_data(&s->ctx, &iter)) != NULL) {
    if (pkt->kind != AOM_CODEC_CX_FRAME_PKT) continue;
    if (append(s, pkt->data.frame.buf, pkt->data.frame.sz, &used) != 0) {
      snprintf(s->error, sizeof s->error, "out of memory for a %zu-byte packet", pkt->data.frame.sz);
      return -1;
    }
    if (pkt->data.frame.flags & AOM_FRAME_IS_KEY) key = 1;
  }
  *out = s->out;
  *out_len = used;
  *is_key = key;
  return 0;
}

// Moves the rate controller's target without rebuilding the encoder: no
// keyframe, the stream goes on referencing what it already has.
int aom_shim_set_bitrate(aom_shim *s, int kbps) {
  aom_codec_enc_cfg_t cfg = s->cfg;
  cfg.rc_target_bitrate = (unsigned)kbps;
  if (aom_codec_enc_config_set(&s->ctx, &cfg) != AOM_CODEC_OK) {
    remember_error(s, "aom_codec_enc_config_set");
    return -1;
  }
  s->cfg = cfg;
  return 0;
}

// The rate controller's current target, as libaom holds it.
int aom_shim_bitrate(const aom_shim *s) { return (int)s->cfg.rc_target_bitrate; }

const char *aom_shim_refused(const aom_shim *s) { return s->refused; }

const char *aom_shim_error(const aom_shim *s) { return s->error; }

void aom_shim_close(aom_shim *s) {
  if (!s) return;
  aom_codec_destroy(&s->ctx);
  free(s->out);
  free(s);
}

const char *aom_shim_version(void) { return aom_codec_version_str(); }

// Minimal SVT-AV1 real-time encoder wrapper for the codec bench.
//
// `rtc` low delay with CBR: SVT-AV1's own real-time coding mode. In low delay
// `get_packet` blocks until the packet of the picture just sent is ready, so
// the latency the bench measures around one `svt_shim_encode` call is the real
// capture-to-bitstream time, pipeline included.
//
// The structure is rtc's flat IPPP (hierarchical-levels 0), the fastest one
// and the one built for real-time coding. In SVT-AV1 4.2.0 it segfaults once
// rate control reaches qindex 0, AV1's lossless mode — every 2560x1440 desktop
// run within seconds, one 1080p run in ~50 — so the lowest QP is held at 1,
// which only removes lossless (docs/research/software-av1.md).
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "EbSvtAv1Enc.h"

#ifdef _MSC_VER
#define strtok_r strtok_s
#define strdup _strdup
#endif

typedef struct svt_shim {
  EbComponentType *handle;
  int w, h;
  uint8_t *out;
  size_t out_cap;
} svt_shim;

static int set(EbSvtAv1EncConfiguration *cfg, const char *name, const char *value, char *err, int errlen) {
  if (svt_av1_enc_parse_parameter(cfg, name, value) != EB_ErrorNone) {
    snprintf(err, errlen, "svt: parameter %s=%s refused", name, value);
    return -1;
  }
  return 0;
}

// `extra` is "name=value;name=value" applied after the defaults below, so an
// experiment can override any of them without a rebuild.
svt_shim *svt_shim_open(int w, int h, int fps, int kbps, int preset, int screen, int lp, int min_q,
                        int max_q, const char *extra, char *err, int errlen) {
  svt_shim *s = (svt_shim *)calloc(1, sizeof(svt_shim));
  if (!s) return NULL;
  s->w = w;
  s->h = h;
  EbSvtAv1EncConfiguration cfg;
  memset(&cfg, 0, sizeof(cfg));
  if (svt_av1_enc_init_handle(&s->handle, &cfg) != EB_ErrorNone) {
    snprintf(err, errlen, "svt_av1_enc_init_handle failed");
    free(s);
    return NULL;
  }
  cfg.source_width = (uint32_t)w;
  cfg.source_height = (uint32_t)h;
  cfg.frame_rate_numerator = (uint32_t)fps;
  cfg.frame_rate_denominator = 1;
  cfg.encoder_bit_depth = 8;

  char buf[32];
#define SETI(name, v)                                              \
  do {                                                             \
    snprintf(buf, sizeof buf, "%d", (int)(v));                     \
    if (set(&cfg, name, buf, err, errlen) != 0) goto fail;         \
  } while (0)
  SETI("preset", preset);
  SETI("rtc", 1);
  SETI("pred-struct", 1);
  SETI("rc", 2);
  SETI("tbr", kbps);
  SETI("buf-sz", 1000);
  SETI("buf-initial-sz", 600);
  SETI("buf-optimal-sz", 600);
  SETI("undershoot-pct", 50);
  SETI("overshoot-pct", 50);
  SETI("hierarchical-levels", 0);
  SETI("min-qp", min_q < 1 ? 1 : min_q);
  SETI("max-qp", max_q);
  SETI("scm", screen ? 1 : 0);
  SETI("lp", lp);
  SETI("lookahead", 0);
  SETI("enable-tf", 0);
  SETI("scd", 0);
  SETI("keyint", 100000);
#undef SETI

  if (extra && *extra) {
    char *copy = strdup(extra);
    char *save = NULL;
    for (char *kv = strtok_r(copy, ";", &save); kv; kv = strtok_r(NULL, ";", &save)) {
      char *eq = strchr(kv, '=');
      if (!eq) continue;
      *eq = 0;
      if (set(&cfg, kv, eq + 1, err, errlen) != 0) {
        free(copy);
        goto fail;
      }
    }
    free(copy);
  }

  if (svt_av1_enc_set_parameter(s->handle, &cfg) != EB_ErrorNone) {
    snprintf(err, errlen, "svt_av1_enc_set_parameter failed");
    goto fail;
  }
  if (svt_av1_enc_init(s->handle) != EB_ErrorNone) {
    snprintf(err, errlen, "svt_av1_enc_init failed");
    goto fail;
  }
  return s;
fail:
  svt_av1_enc_deinit_handle(s->handle);
  free(s);
  return NULL;
}

int svt_shim_encode(svt_shim *s, const uint8_t *y, const uint8_t *u, const uint8_t *v, int y_stride,
                    int uv_stride, int64_t pts, int force_kf, const uint8_t **out, size_t *out_len,
                    int *is_key) {
  EbSvtIOFormat io;
  io.luma = (uint8_t *)y;
  io.cb = (uint8_t *)u;
  io.cr = (uint8_t *)v;
  io.y_stride = (uint32_t)y_stride;
  io.cb_stride = (uint32_t)uv_stride;
  io.cr_stride = (uint32_t)uv_stride;

  EbBufferHeaderType in;
  memset(&in, 0, sizeof(in));
  in.size = sizeof(in);
  in.p_buffer = (uint8_t *)&io;
  in.n_filled_len = (uint32_t)(y_stride * s->h + uv_stride * (s->h / 2) * 2);
  in.n_alloc_len = in.n_filled_len;
  in.pts = pts;
  in.pic_type = force_kf ? EB_AV1_KEY_PICTURE : EB_AV1_INVALID_PICTURE;
  if (svt_av1_enc_send_picture(s->handle, &in) != EB_ErrorNone) return -1;

  EbBufferHeaderType *pkt = NULL;
  EbErrorType ret = svt_av1_enc_get_packet(s->handle, &pkt, 0);
  if (ret == EB_NoErrorEmptyQueue || !pkt) {
    *out_len = 0;
    *is_key = 0;
    *out = s->out;
    return 0;
  }
  if (ret != EB_ErrorNone) return -1;
  size_t len = pkt->n_filled_len;
  if (len > s->out_cap) {
    uint8_t *grown = (uint8_t *)realloc(s->out, len * 2);
    if (!grown) {
      svt_av1_enc_release_out_buffer(&pkt);
      return -1;
    }
    s->out = grown;
    s->out_cap = len * 2;
  }
  memcpy(s->out, pkt->p_buffer, len);
  *is_key = pkt->pic_type == EB_AV1_KEY_PICTURE;
  svt_av1_enc_release_out_buffer(&pkt);
  *out = s->out;
  *out_len = len;
  return 0;
}

void svt_shim_close(svt_shim *s) {
  if (!s) return;
  // End the stream properly and drain it, as the library asks before deinit.
  EbBufferHeaderType eos;
  memset(&eos, 0, sizeof(eos));
  eos.size = sizeof(eos);
  eos.flags = EB_BUFFERFLAG_EOS;
  eos.pic_type = EB_AV1_INVALID_PICTURE;
  if (svt_av1_enc_send_picture(s->handle, &eos) == EB_ErrorNone) {
    for (;;) {
      EbBufferHeaderType *pkt = NULL;
      if (svt_av1_enc_get_packet(s->handle, &pkt, 1) != EB_ErrorNone || !pkt) break;
      int last = (pkt->flags & EB_BUFFERFLAG_EOS) != 0;
      svt_av1_enc_release_out_buffer(&pkt);
      if (last) break;
    }
  }
  svt_av1_enc_deinit(s->handle);
  svt_av1_enc_deinit_handle(s->handle);
  free(s->out);
  free(s);
}

const char *svt_shim_version(void) { return svt_av1_get_version(); }

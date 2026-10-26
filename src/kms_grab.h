/* KMS scanout capture: see kms_grab.c. Linux only. */
#ifndef TP_KMS_GRAB_H
#define TP_KMS_GRAB_H

#include <stdint.h>

/* An active CRTC: its mode and the connector showing it. */
typedef struct {
    uint32_t crtc_id;
    uint32_t width, height;
    uint32_t refresh_mhz;
    char name[32];
} TpKmsOutput;

/* The framebuffer a CRTC shows now, as DMA-BUFs owned by the caller (close
 * them with tp_kms_frame_close). `modifier` is DRM_FORMAT_MOD_INVALID when
 * the framebuffer was created without one. */
typedef struct {
    int32_t fds[4];
    /* Plane i lives in the buffer object of plane source_plane[i] (itself,
     * or an earlier plane sharing it; then fds[i] is -1). */
    int32_t source_plane[4];
    uint32_t offsets[4], pitches[4];
    int32_t planes;
    uint32_t fourcc;
    uint64_t modifier;
    uint32_t width, height;
} TpKmsFrame;

/* A DRM device node (/dev/dri/cardN): fd, or -errno. */
int tp_kms_open(const char *path);
/* Active CRTCs, at most `capacity`: count, or -errno. */
int tp_kms_outputs(int fd, TpKmsOutput *outputs, int capacity);
/* 0, -EPERM without DRM master or CAP_SYS_ADMIN, -ENOENT when the CRTC
 * shows nothing, or another -errno. */
int tp_kms_grab(int fd, uint32_t crtc_id, TpKmsFrame *frame);
void tp_kms_frame_close(TpKmsFrame *frame);

#endif

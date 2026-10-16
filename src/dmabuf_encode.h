/* Host zero-copy capture: DMA-BUF frames converted on the GPU straight into
 * an encoder's hardware frames (VA-API surfaces or CUDA memory); see
 * dmabuf_encode.c. Linux only.
 */
#ifndef TP_DMABUF_ENCODE_H
#define TP_DMABUF_ENCODE_H

#include <libavutil/buffer.h>
#include <libavutil/frame.h>
#include <stdint.h>

/* One captured DMA-BUF frame: a single-layer RGB DRM format (XR24, AR24,
 * XB24 or AB24) with up to four planes (modifiers may add auxiliary ones).
 * File descriptors stay owned by the caller and must stay open (and the
 * buffer unmodified) until the call that receives this returns. */
typedef struct {
    int32_t fds[4];
    uint32_t offsets[4], pitches[4];
    int32_t planes;
    uint32_t fourcc;
    uint64_t modifier;
    int32_t width, height;
    /* Visible region within the buffer (PipeWire crop metadata). */
    int32_t crop_x, crop_y, crop_width, crop_height;
} TpDmabuf;

typedef struct TpImport TpImport;

/* Modifiers the encoder's device can import for `fourcc`, at most `capacity`
 * of them. Returns the count (0: no zero-copy for this format) or an error.
 * `frames` is the encoder's VA-API or CUDA NV12 frames context, `node` the
 * VA-API render node (NULL for CUDA). Creates `*import` on first use. */
int tp_import_modifiers(TpImport **import, AVBufferRef *frames, const char *node, uint32_t fourcc,
    uint64_t *modifiers, int capacity);

/* Colour-convert (limited-range BT.709 NV12) and scale `source` to the size
 * of `frames` on the GPU, into a new frame from that pool written to
 * `output`. All GPU reads of `source` are complete when this returns. */
int tp_import_convert(TpImport **import, AVBufferRef *frames, const char *node,
    const TpDmabuf *source, AVFrame *output);

void tp_import_free(TpImport *import);

/* Tests and benchmarks: an RGBA texture exported as a DMA-BUF from the EGL
 * device for render node `node` (or the NVIDIA device when NULL), filled
 * with four solid quadrants (`colors`: 4 x RGB, top-left, top-right,
 * bottom-left, bottom-right). */
typedef struct TpTestDmabuf TpTestDmabuf;
TpTestDmabuf *tp_test_dmabuf_new(const char *node, int width, int height, const uint8_t *colors,
    TpDmabuf *out, int *error);
void tp_test_dmabuf_free(TpTestDmabuf *source);

#endif

/* Zero-copy presentation of NVDEC (FFmpeg CUDA) frames in an OpenGL context.
 *
 * The decoded NV12 surface never leaves the GPU: CUDA copies its two planes
 * device-to-device into GL textures registered for interop, and the shared
 * NV12 shader (gl_nv12.c) converts limited-range BT.709 to RGB while drawing
 * a quad into the current framebuffer. Replaces download + CPU copies +
 * texture upload (measured 0.67 -> 0.22 ms at 1080p, 2.78 -> 0.27 ms at 4K
 * for the transfer alone on an RTX 3080).
 *
 * GL entry points come from a caller-supplied resolver (SDL's in the app,
 * EGL's in the self-test) and libcuda is loaded dynamically by FFmpeg's
 * nv-codec-headers loader, so nothing new is linked. All GL state touched is
 * saved and restored: the host renderer (SDL) caches its own state.
 */
#include "gl_nv12.h"
#include <ffnvcodec/dynlink_loader.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_cuda.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>

typedef struct TpCudaGl {
    TpNv12Gl nv12;
    CudaFunctions *cu;
    int width, height;
    CUcontext registered_for;
    CUgraphicsResource resources[2];
} TpCudaGl;

static void unregister(TpCudaGl *g) {
    for (int i = 0; i < 2; i++)
        if (g->resources[i]) {
            g->cu->cuGraphicsUnregisterResource(g->resources[i]);
            g->resources[i] = NULL;
        }
    g->width = g->height = 0;
    g->registered_for = NULL;
}

void tp_cuda_gl_free(TpCudaGl *g) {
    if (!g) return;
    if (g->cu) {
        unregister(g);
        cuda_free_functions(&g->cu);
    }
    tp_nv12_gl_destroy(&g->nv12);
    free(g);
}

/* Create a presenter in the current GL context. NULL if GL 2.1 shaders or
 * libcuda are unavailable; the caller keeps its CPU presentation path. */
TpCudaGl *tp_cuda_gl_new(TpGetProc get) {
    TpCudaGl *g = calloc(1, sizeof *g);
    if (!g || !get || cuda_load_functions(&g->cu, NULL) != 0) {
        tp_cuda_gl_free(g);
        return NULL;
    }
    /* CUDA can only register textures of an NVIDIA GL context (not Mesa,
     * llvmpipe or another vendor's GPU in a hybrid system). */
    const unsigned char *(*get_string)(GLenum) = (const unsigned char *(*)(GLenum))get("glGetString");
    const char *vendor = get_string ? (const char *)get_string(GL_VENDOR) : NULL;
    if (!vendor || !strstr(vendor, "NVIDIA") || tp_nv12_gl_init(&g->nv12, get) != 0) {
        tp_cuda_gl_free(g);
        return NULL;
    }
    return g;
}

/* (Re)allocate texture storage and register it with CUDA. Runs inside
 * tp_nv12_gl_draw's saved-state region. */
static int ensure_textures(TpCudaGl *g, CUcontext ctx, int width, int height) {
    if (g->width == width && g->height == height && g->registered_for == ctx) return 0;
    TpGl *gl = &g->nv12.gl;
    unregister(g);
    for (int i = 0; i < 2; i++) {
        gl->BindTexture(GL_TEXTURE_2D, g->nv12.textures[i]);
        gl->TexImage2D(GL_TEXTURE_2D, 0, i ? GL_RG8 : GL_R8, i ? width / 2 : width,
            i ? height / 2 : height, 0, i ? GL_RG : GL_RED, GL_UNSIGNED_BYTE, NULL);
    }
    if (gl->GetError() != GL_NO_ERROR) return AVERROR_EXTERNAL;
    for (int i = 0; i < 2; i++) {
        if (g->cu->cuGraphicsGLRegisterImage(&g->resources[i], g->nv12.textures[i], GL_TEXTURE_2D,
                CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD) != CUDA_SUCCESS) {
            unregister(g);
            return AVERROR_EXTERNAL;
        }
    }
    g->width = width; g->height = height; g->registered_for = ctx;
    return 0;
}

/* Copy the frame's planes into the interop textures (GPU to GPU). */
static int upload(void *presenter, const AVFrame *frame) {
    TpCudaGl *g = presenter;
    if (frame->format != AV_PIX_FMT_CUDA || !frame->hw_frames_ctx) return AVERROR(EINVAL);
    AVHWFramesContext *frames = (AVHWFramesContext *)frame->hw_frames_ctx->data;
    if (frames->sw_format != AV_PIX_FMT_NV12) return AVERROR(ENOSYS); /* 10-bit/HDR: CPU path */
    if ((frame->width | frame->height) & 1) return AVERROR(EINVAL);
    AVCUDADeviceContext *device = frames->device_ctx->hwctx;
    CudaFunctions *cu = g->cu;
    if (cu->cuCtxPushCurrent(device->cuda_ctx) != CUDA_SUCCESS) return AVERROR_EXTERNAL;
    int result = ensure_textures(g, device->cuda_ctx, frame->width, frame->height);
    if (result == 0 && cu->cuGraphicsMapResources(2, g->resources, 0) != CUDA_SUCCESS)
        result = AVERROR_EXTERNAL;
    if (result == 0) {
        for (int plane = 0; plane < 2 && result == 0; plane++) {
            CUarray array;
            if (cu->cuGraphicsSubResourceGetMappedArray(&array, g->resources[plane], 0, 0) != CUDA_SUCCESS) {
                result = AVERROR_EXTERNAL;
                break;
            }
            CUDA_MEMCPY2D copy = {0};
            copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
            copy.srcDevice = (CUdeviceptr)frame->data[plane];
            copy.srcPitch = frame->linesize[plane];
            copy.dstMemoryType = CU_MEMORYTYPE_ARRAY;
            copy.dstArray = array;
            copy.WidthInBytes = frame->width; /* UV: width/2 texels of 2 bytes */
            copy.Height = plane ? frame->height / 2 : frame->height;
            if (cu->cuMemcpy2D(&copy) != CUDA_SUCCESS) result = AVERROR_EXTERNAL;
        }
        cu->cuGraphicsUnmapResources(2, g->resources, 0);
    }
    CUcontext ignored;
    cu->cuCtxPopCurrent(&ignored);
    return result;
}

/* Draw `frame` into the current framebuffer at (x, y, width, height) in
 * framebuffer pixels with a top-left origin (`framebuffer_height` flips it
 * for GL). Restores every piece of GL state it changes. */
int tp_cuda_gl_draw(TpCudaGl *g, const AVFrame *frame, int x, int y, int width, int height,
    int framebuffer_height) {
    if (!g) return AVERROR(EINVAL);
    return tp_nv12_gl_draw(&g->nv12, upload, g, frame, x, y, width, height, framebuffer_height);
}

/* ---- Offscreen self-test (no window, no linked EGL): used by cargo tests. ---- */

static int match_nvidia(void *context, const char *vendor, const char *node) {
    (void)context; (void)node;
    return strstr(vendor, "NVIDIA") != NULL;
}

static int draw_cuda(void *context, TpGetProc get) {
    const AVFrame *frame = context;
    TpCudaGl *g = tp_cuda_gl_new(get);
    if (!g) return AVERROR_EXTERNAL;
    int result = tp_cuda_gl_draw(g, frame, 0, 0, frame->width, frame->height, frame->height);
    tp_cuda_gl_free(g);
    return result;
}

/* Draw a CUDA frame into an offscreen RGBA framebuffer on an NVIDIA EGL
 * device and read it back. Returns 0 and fills `rgba` (width*height*4),
 * AVERROR(ENODEV) when no NVIDIA EGL device exists, or an error. */
int tp_cuda_gl_self_test(const AVFrame *frame, uint8_t *rgba) {
    if (!frame) return AVERROR(EINVAL);
    return tp_egl_offscreen(match_nvidia, draw_cuda, (void *)frame, frame->width, frame->height, rgba);
}

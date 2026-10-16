/* Zero-copy presentation of VA-API (Intel/AMD) decoded frames in an EGL
 * OpenGL context.
 *
 * The decoded NV12 surface is exported as DRM PRIME dma-bufs
 * (av_hwframe_map -> vaExportSurfaceHandle with separate layers), each layer
 * (R8 luma, GR88 chroma) is imported as an EGLImage with
 * EGL_EXT_image_dma_buf_import (+ explicit format modifiers when
 * EGL_EXT_image_dma_buf_import_modifiers is available), bound to the shared
 * NV12 textures with glEGLImageTargetTexture2DOES and drawn by the shared
 * BT.709 shader (gl_nv12.c). The GPU samples the decoder's memory directly.
 *
 * Requirements, checked at creation: the current GL context is an EGL
 * context (GLX cannot import dma-bufs), its display exposes the dma-buf
 * import extension and identifies its DRM render node (EGL_EXT_device_query),
 * and GL_OES_EGL_image is available. The caller must open the VA-API decoder
 * on that same render node (tp_vaapi_egl_render_node), so the import never
 * crosses GPUs. libEGL is resolved at runtime; nothing new is linked.
 */
#define _POSIX_C_SOURCE 200809L /* stat, opendir, clock_gettime under -std=c11 */
#include "gl_nv12.h"
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixdesc.h>
#include <dirent.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>

typedef void *EGLDisplay;
typedef void *EGLContext;
typedef void *EGLImage;
typedef void *EGLDeviceEXT;
typedef int EGLint;
typedef intptr_t EGLAttrib;
typedef unsigned int EGLBoolean;

#define EGL_NONE 0x3038
#define EGL_WIDTH 0x3057
#define EGL_HEIGHT 0x3056
#define EGL_EXTENSIONS 0x3055
#define EGL_DEVICE_EXT 0x322C
#define EGL_DRM_DEVICE_FILE_EXT 0x3233
#define EGL_DRM_RENDER_NODE_FILE_EXT 0x3377
#define EGL_LINUX_DMA_BUF_EXT 0x3270
#define EGL_LINUX_DRM_FOURCC_EXT 0x3271
#define DRM_FORMAT_MOD_LINEAR 0ULL
#define DRM_FORMAT_MOD_INVALID 0x00ffffffffffffffULL
#define FOURCC(a, b, c, d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define DRM_FORMAT_R8 FOURCC('R', '8', ' ', ' ')
#define DRM_FORMAT_GR88 FOURCC('G', 'R', '8', '8')

/* Per-plane FD, OFFSET, PITCH, MODIFIER_LO, MODIFIER_HI attribute names. */
static const EGLint PLANE_ATTRIBUTES[4][5] = {
    {0x3272, 0x3273, 0x3274, 0x3443, 0x3444},
    {0x3275, 0x3276, 0x3277, 0x3445, 0x3446},
    {0x3278, 0x3279, 0x327A, 0x3447, 0x3448},
    {0x3440, 0x3441, 0x3442, 0x3449, 0x344A},
};

typedef struct {
    EGLDisplay (*GetCurrentDisplay)(void);
    EGLContext (*GetCurrentContext)(void);
    const char *(*QueryString)(EGLDisplay, EGLint);
    EGLImage (*CreateImageKHR)(EGLDisplay, EGLContext, unsigned, void *, const EGLint *);
    EGLBoolean (*DestroyImageKHR)(EGLDisplay, EGLImage);
    EGLBoolean (*QueryDisplayAttribEXT)(EGLDisplay, EGLint, EGLAttrib *);
    const char *(*QueryDeviceStringEXT)(EGLDeviceEXT, EGLint);
} Egl;

typedef struct TpVaapiEgl {
    TpNv12Gl nv12;
    Egl egl;
    void (*image_target)(GLenum, void *);
    EGLDisplay display;
    int modifiers;
    char node[64];
} TpVaapiEgl;

static int has_extension(const char *list, const char *name) {
    size_t length = strlen(name);
    for (const char *at = list; at && (at = strstr(at, name)); at += length)
        if ((at == list || at[-1] == ' ') && (at[length] == ' ' || at[length] == '\0')) return 1;
    return 0;
}

/* Map a primary DRM node (/dev/dri/cardN) to its render node via sysfs. */
static int render_node_for(const char *path, char *out, size_t length) {
    struct stat info;
    if (strlen(path) >= length || stat(path, &info) != 0 || !S_ISCHR(info.st_mode)) return 0;
    if (major(info.st_rdev) == 226 && minor(info.st_rdev) >= 128) {
        snprintf(out, length, "%s", path);
        return 1;
    }
    char directory[128];
    snprintf(directory, sizeof directory, "/sys/dev/char/%u:%u/device/drm",
        major(info.st_rdev), minor(info.st_rdev));
    DIR *dir = opendir(directory);
    if (!dir) return 0;
    int found = 0;
    for (struct dirent *entry; !found && (entry = readdir(dir));)
        if (!strncmp(entry->d_name, "renderD", 7)) {
            snprintf(out, length, "/dev/dri/%.32s", entry->d_name);
            found = 1;
        }
    closedir(dir);
    return found;
}

void tp_vaapi_egl_free(TpVaapiEgl *g) {
    if (!g) return;
    tp_nv12_gl_destroy(&g->nv12);
    free(g);
}

/* Create a presenter in the current GL context. NULL unless that context is
 * EGL with dma-buf import on a DRM device with a render node; the caller then
 * keeps its CPU presentation path. */
TpVaapiEgl *tp_vaapi_egl_new(TpGetProc get) {
    TpVaapiEgl *g = calloc(1, sizeof *g);
    if (!g || !get) goto fail;
    Egl *egl = &g->egl;
#define LOAD(name) if (!(*(void **)&egl->name = tp_egl_proc("egl" #name))) goto fail;
    LOAD(GetCurrentDisplay) LOAD(GetCurrentContext) LOAD(QueryString) LOAD(CreateImageKHR)
    LOAD(DestroyImageKHR) LOAD(QueryDisplayAttribEXT) LOAD(QueryDeviceStringEXT)
#undef LOAD
    /* GLX contexts (SDL's X11 default) report no current EGL context. */
    if (!egl->GetCurrentContext()) goto fail;
    g->display = egl->GetCurrentDisplay();
    if (!g->display) goto fail;
    const char *extensions = egl->QueryString(g->display, EGL_EXTENSIONS);
    if (!has_extension(extensions, "EGL_EXT_image_dma_buf_import")
        || !has_extension(extensions, "EGL_KHR_image_base")) goto fail;
    g->modifiers = has_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers");
    EGLAttrib device = 0;
    if (!egl->QueryDisplayAttribEXT(g->display, EGL_DEVICE_EXT, &device) || !device) goto fail;
    const char *node = egl->QueryDeviceStringEXT((EGLDeviceEXT)device, EGL_DRM_RENDER_NODE_FILE_EXT);
    if (!node) node = egl->QueryDeviceStringEXT((EGLDeviceEXT)device, EGL_DRM_DEVICE_FILE_EXT);
    /* Software rasterizers (llvmpipe) expose no DRM node and stop here. */
    if (!node || !render_node_for(node, g->node, sizeof g->node)) goto fail;
    g->image_target = (void (*)(GLenum, void *))get("glEGLImageTargetTexture2DOES");
    if (!g->image_target || tp_nv12_gl_init(&g->nv12, get) != 0) goto fail;
    return g;
fail:
    tp_vaapi_egl_free(g);
    return NULL;
}

/* The DRM render node of the presenter's EGL display; open the VA-API
 * decoder on exactly this node. */
const char *tp_vaapi_egl_render_node(const TpVaapiEgl *g) { return g ? g->node : NULL; }

static int import_layer(TpVaapiEgl *g, const AVDRMFrameDescriptor *d, int index, uint32_t fourcc,
    int width, int height, GLuint texture) {
    const AVDRMLayerDescriptor *layer = &d->layers[index];
    if (layer->format != fourcc || layer->nb_planes < 1 || layer->nb_planes > 4) return AVERROR(ENOSYS);
    EGLint attributes[7 + 4 * 10 + 1];
    int n = 0;
    attributes[n++] = EGL_WIDTH; attributes[n++] = width;
    attributes[n++] = EGL_HEIGHT; attributes[n++] = height;
    attributes[n++] = EGL_LINUX_DRM_FOURCC_EXT; attributes[n++] = (EGLint)fourcc;
    for (int p = 0; p < layer->nb_planes; p++) {
        const AVDRMPlaneDescriptor *plane = &layer->planes[p];
        if (plane->object_index < 0 || plane->object_index >= d->nb_objects) return AVERROR(EINVAL);
        const AVDRMObjectDescriptor *object = &d->objects[plane->object_index];
        uint64_t modifier = object->format_modifier;
        /* Without the modifiers extension only linear or implicit layouts
         * can be described; a tiled surface would be misread. */
        if (!g->modifiers && modifier != DRM_FORMAT_MOD_LINEAR && modifier != DRM_FORMAT_MOD_INVALID)
            return AVERROR(ENOSYS);
        if (p > 0 && !g->modifiers) return AVERROR(ENOSYS);
        attributes[n++] = PLANE_ATTRIBUTES[p][0]; attributes[n++] = object->fd;
        attributes[n++] = PLANE_ATTRIBUTES[p][1]; attributes[n++] = (EGLint)plane->offset;
        attributes[n++] = PLANE_ATTRIBUTES[p][2]; attributes[n++] = (EGLint)plane->pitch;
        if (g->modifiers && modifier != DRM_FORMAT_MOD_INVALID) {
            attributes[n++] = PLANE_ATTRIBUTES[p][3]; attributes[n++] = (EGLint)(modifier & 0xffffffff);
            attributes[n++] = PLANE_ATTRIBUTES[p][4]; attributes[n++] = (EGLint)(modifier >> 32);
        }
    }
    attributes[n] = EGL_NONE;
    EGLImage image = g->egl.CreateImageKHR(g->display, NULL, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
    if (!image) return AVERROR_EXTERNAL;
    g->nv12.gl.BindTexture(GL_TEXTURE_2D, texture);
    g->image_target(GL_TEXTURE_2D, image);
    /* The texture keeps its own reference to the imported buffer. */
    g->egl.DestroyImageKHR(g->display, image);
    return g->nv12.gl.GetError() == GL_NO_ERROR ? 0 : AVERROR_EXTERNAL;
}

/* Import the frame's planes as the shared textures' storage (no copy). */
static int upload(void *presenter, const AVFrame *frame) {
    TpVaapiEgl *g = presenter;
    if (frame->format != AV_PIX_FMT_VAAPI || !frame->hw_frames_ctx) return AVERROR(EINVAL);
    AVHWFramesContext *frames = (AVHWFramesContext *)frame->hw_frames_ctx->data;
    if (frames->sw_format != AV_PIX_FMT_NV12) return AVERROR(ENOSYS); /* 10-bit/HDR: CPU path */
    if ((frame->width | frame->height) & 1) return AVERROR(EINVAL);
    AVFrame *mapped = av_frame_alloc();
    if (!mapped) return AVERROR(ENOMEM);
    mapped->format = AV_PIX_FMT_DRM_PRIME;
    /* Synchronises with the decoder and exports separate R8 / GR88 layers. */
    int result = av_hwframe_map(mapped, frame, AV_HWFRAME_MAP_READ);
    if (result == 0) {
        const AVDRMFrameDescriptor *d = (const AVDRMFrameDescriptor *)mapped->data[0];
        if (!d || d->nb_layers != 2) result = AVERROR(ENOSYS);
        if (result == 0)
            result = import_layer(g, d, 0, DRM_FORMAT_R8, frame->width, frame->height, g->nv12.textures[0]);
        if (result == 0)
            result = import_layer(g, d, 1, DRM_FORMAT_GR88, frame->width / 2, frame->height / 2,
                g->nv12.textures[1]);
    }
    /* Closes the exported fds; the EGL imports hold their own references. */
    av_frame_free(&mapped);
    return result;
}

/* Draw `frame` into the current framebuffer at (x, y, width, height) in
 * framebuffer pixels with a top-left origin. Restores all GL state. */
int tp_vaapi_egl_draw(TpVaapiEgl *g, const AVFrame *frame, int x, int y, int width, int height,
    int framebuffer_height) {
    if (!g) return AVERROR(EINVAL);
    return tp_nv12_gl_draw(&g->nv12, upload, g, frame, x, y, width, height, framebuffer_height);
}

/* ---- Offscreen self-test (no window, no linked EGL): used by cargo tests. ---- */

typedef struct {
    const char *node;
    const AVFrame *frame;
    int iterations;
    int64_t draw_ns, download_ns;
} SelfTest;

static int64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

static int match_node(void *context, const char *vendor, const char *node) {
    (void)vendor;
    return node && !strcmp(node, ((SelfTest *)context)->node);
}

static int draw_vaapi(void *context, TpGetProc get) {
    SelfTest *test = context;
    const AVFrame *frame = test->frame;
    TpVaapiEgl *g = tp_vaapi_egl_new(get);
    if (!g) return AVERROR_EXTERNAL;
    void (*finish)(void) = (void (*)(void))get("glFinish");
    int result = strcmp(g->node, test->node) || !finish ? AVERROR_EXTERNAL : 0;
    /* First draw is the checked one; the rest time import + draw + finish. */
    for (int i = 0; result == 0 && i <= test->iterations; i++) {
        int64_t start = now_ns();
        result = tp_vaapi_egl_draw(g, frame, 0, 0, frame->width, frame->height, frame->height);
        finish();
        if (i > 0) test->draw_ns += now_ns() - start;
    }
    tp_vaapi_egl_free(g);
    /* Reference: what the CPU path pays just to download the same surface. */
    for (int i = 0; result == 0 && i < test->iterations; i++) {
        AVFrame *download = av_frame_alloc();
        if (!download) return AVERROR(ENOMEM);
        int64_t start = now_ns();
        result = av_hwframe_transfer_data(download, frame, 0);
        test->download_ns += now_ns() - start;
        av_frame_free(&download);
    }
    return result;
}

/* Draw a VA-API frame into an offscreen RGBA framebuffer on the EGL device
 * for render node `node` (the decoder's device) and read it back. Returns 0
 * and fills `rgba` (width*height*4, bottom-up rows), AVERROR(ENODEV) when no
 * EGL device exposes that node, or an error. With `iterations` > 0 also
 * reports the mean import+draw (`draw_ns`) and plain download (`download_ns`)
 * times per frame. */
int tp_vaapi_egl_self_test(const char *node, const AVFrame *frame, uint8_t *rgba, int iterations,
    int64_t *draw_ns, int64_t *download_ns) {
    if (!node || !frame || iterations < 0) return AVERROR(EINVAL);
    SelfTest test = {.node = node, .frame = frame, .iterations = iterations};
    int result = tp_egl_offscreen(match_node, draw_vaapi, &test, frame->width, frame->height, rgba);
    if (iterations > 0) {
        if (draw_ns) *draw_ns = test.draw_ns / iterations;
        if (download_ns) *download_ns = test.download_ns / iterations;
    }
    return result;
}

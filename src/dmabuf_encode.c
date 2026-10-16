/* Host zero-copy capture: a captured DMA-BUF (PipeWire / compositor memory)
 * becomes an encoder input frame without touching the CPU.
 *
 * VA-API (Intel/AMD): the DMA-BUF is imported as a VA surface
 * (vaCreateSurfaces with a DRM PRIME 2 descriptor) and the video processor
 * (VPP) scales it and converts RGB to limited-range BT.709 NV12 directly into
 * a surface from the encoder's own pool.
 *
 * NVIDIA: the DMA-BUF is imported as an EGLImage on the NVIDIA EGL device of
 * the encoder's CUDA context, sampled as an external texture by a GLES 3
 * shader that scales and converts to NV12 (luma into an R8 texture, chroma
 * into a half-size RG8 texture), and CUDA copies those textures (registered
 * for interop) into a CUDA frame from NVENC's pool. NVENC also accepts RGB
 * input directly, but it converts with its own fixed matrix and cannot
 * scale; the shader keeps the colours identical to every other path.
 *
 * libEGL, GLES and libcuda are resolved at runtime (gl_nv12.c, nv-codec
 * headers); libva comes with FFmpeg's VA-API support. The caller holds the
 * GStreamer buffer (and so the compositor's DMA-BUF) until we return, and we
 * return only after the GPU has finished reading it.
 */
#define _POSIX_C_SOURCE 200809L
#include "dmabuf_encode.h"
#include "gl_nv12.h"
#include <ffnvcodec/dynlink_loader.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_cuda.h>
#include <libavutil/hwcontext_vaapi.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

typedef void *EGLDisplay;
typedef void *EGLContext;
typedef void *EGLConfig;
typedef void *EGLImage;
typedef void *EGLDeviceEXT;
typedef int EGLint;
typedef intptr_t EGLAttrib;
typedef unsigned int EGLBoolean;
typedef uint64_t EGLuint64KHR;

#define EGL_NONE 0x3038
#define EGL_WIDTH 0x3057
#define EGL_HEIGHT 0x3056
#define EGL_EXTENSIONS 0x3055
#define EGL_VENDOR 0x3053
#define EGL_DONT_CARE (-1)
#define EGL_SURFACE_TYPE 0x3033
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_ES3_BIT 0x0040
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_CONTEXT_MAJOR_VERSION 0x3098
#define EGL_PLATFORM_DEVICE_EXT 0x313F
#define EGL_DRM_RENDER_NODE_FILE_EXT 0x3377
#define EGL_CUDA_DEVICE_NV 0x323A
#define EGL_LINUX_DMA_BUF_EXT 0x3270
#define EGL_LINUX_DRM_FOURCC_EXT 0x3271
#define EGL_GL_TEXTURE_2D 0x30B1

#define GL_TEXTURE_EXTERNAL_OES 0x8D65
#define GL_FRAMEBUFFER 0x8D40
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5
#define GL_VERTEX_SHADER 0x8B31
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_TRIANGLE_STRIP 0x0005
#define GL_NEAREST 0x2600
#define GL_RGBA8 0x8058
#define GL_RGBA 0x1908
#define GL_SCISSOR_TEST 0x0C11
#define GL_COLOR_BUFFER_BIT 0x00004000

#define FOURCC(a, b, c, d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define DRM_FORMAT_XRGB8888 FOURCC('X', 'R', '2', '4')
#define DRM_FORMAT_ARGB8888 FOURCC('A', 'R', '2', '4')
#define DRM_FORMAT_XBGR8888 FOURCC('X', 'B', '2', '4')
#define DRM_FORMAT_ABGR8888 FOURCC('A', 'B', '2', '4')
#define DRM_FORMAT_MOD_INVALID 0x00ffffffffffffffULL

/* Per-plane FD, OFFSET, PITCH, MODIFIER_LO, MODIFIER_HI attribute names. */
static const EGLint PLANE_ATTRIBUTES[4][5] = {
    {0x3272, 0x3273, 0x3274, 0x3443, 0x3444},
    {0x3275, 0x3276, 0x3277, 0x3445, 0x3446},
    {0x3278, 0x3279, 0x327A, 0x3447, 0x3448},
    {0x3440, 0x3441, 0x3442, 0x3449, 0x344A},
};

typedef struct {
    EGLBoolean (*QueryDevicesEXT)(EGLint, EGLDeviceEXT *, EGLint *);
    const char *(*QueryDeviceStringEXT)(EGLDeviceEXT, EGLint);
    EGLBoolean (*QueryDeviceAttribEXT)(EGLDeviceEXT, EGLint, EGLAttrib *);
    EGLDisplay (*GetPlatformDisplayEXT)(EGLint, void *, const EGLint *);
    EGLBoolean (*Initialize)(EGLDisplay, EGLint *, EGLint *);
    const char *(*QueryString)(EGLDisplay, EGLint);
    EGLBoolean (*BindAPI)(unsigned);
    EGLBoolean (*ChooseConfig)(EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *);
    EGLContext (*CreateContext)(EGLDisplay, EGLConfig, EGLContext, const EGLint *);
    EGLBoolean (*DestroyContext)(EGLDisplay, EGLContext);
    EGLBoolean (*MakeCurrent)(EGLDisplay, void *, void *, EGLContext);
    EGLImage (*CreateImageKHR)(EGLDisplay, EGLContext, unsigned, void *, const EGLint *);
    EGLBoolean (*DestroyImageKHR)(EGLDisplay, EGLImage);
    EGLBoolean (*QueryDmaBufModifiersEXT)(EGLDisplay, EGLint, EGLint, EGLuint64KHR *, EGLBoolean *, EGLint *);
    EGLBoolean (*ExportDMABUFImageQueryMESA)(EGLDisplay, EGLImage, int *, int *, EGLuint64KHR *);
    EGLBoolean (*ExportDMABUFImageMESA)(EGLDisplay, EGLImage, int *, EGLint *, EGLint *);
} Egl;

static int load_egl(Egl *egl) {
#define LOAD(name) if (!(*(void **)&egl->name = tp_egl_proc("egl" #name))) return 0;
    LOAD(QueryDevicesEXT) LOAD(QueryDeviceStringEXT) LOAD(QueryDeviceAttribEXT)
    LOAD(GetPlatformDisplayEXT) LOAD(Initialize) LOAD(QueryString) LOAD(BindAPI)
    LOAD(ChooseConfig) LOAD(CreateContext) LOAD(DestroyContext) LOAD(MakeCurrent)
    LOAD(CreateImageKHR) LOAD(DestroyImageKHR) LOAD(QueryDmaBufModifiersEXT)
#undef LOAD
    /* Export is only needed by the test source. */
    *(void **)&egl->ExportDMABUFImageQueryMESA = tp_egl_proc("eglExportDMABUFImageQueryMESA");
    *(void **)&egl->ExportDMABUFImageMESA = tp_egl_proc("eglExportDMABUFImageMESA");
    return 1;
}

static int has_extension(const char *list, const char *name) {
    size_t length = strlen(name);
    for (const char *at = list; at && (at = strstr(at, name)); at += length)
        if ((at == list || at[-1] == ' ') && (at[length] == ' ' || at[length] == '\0')) return 1;
    return 0;
}

/* The initialised EGL display of the device with DRM render node `node`, or
 * (node NULL) the NVIDIA device of CUDA device `cuda_device` (any NVIDIA
 * device when -1). The display needs dma-buf import with modifiers. */
static EGLDisplay find_display(Egl *egl, const char *node, int cuda_device) {
    EGLDeviceEXT devices[16];
    EGLint count = 0;
    if (!egl->QueryDevicesEXT(16, devices, &count)) return NULL;
    for (int i = 0; i < count; i++) {
        if (node) {
            const char *device_node = egl->QueryDeviceStringEXT(devices[i], EGL_DRM_RENDER_NODE_FILE_EXT);
            if (!device_node || strcmp(device_node, node)) continue;
        } else {
            const char *extensions = egl->QueryDeviceStringEXT(devices[i], EGL_EXTENSIONS);
            if (!has_extension(extensions, "EGL_NV_device_cuda")) continue;
            EGLAttrib ordinal = -1;
            if (cuda_device >= 0 && egl->QueryDeviceAttribEXT(devices[i], EGL_CUDA_DEVICE_NV, &ordinal)
                && ordinal != cuda_device) continue;
        }
        EGLDisplay display = egl->GetPlatformDisplayEXT(EGL_PLATFORM_DEVICE_EXT, devices[i], NULL);
        EGLint major, minor;
        if (!display || !egl->Initialize(display, &major, &minor)) continue;
        const char *extensions = egl->QueryString(display, EGL_EXTENSIONS);
        if (!has_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers")
            || !has_extension(extensions, "EGL_KHR_image_base")) continue;
        return display;
    }
    return NULL;
}

/* ---- GLES 3 conversion (NVIDIA) ---- */

typedef struct {
    void (*ActiveTexture)(GLenum);
    void (*AttachShader)(GLuint, GLuint);
    void (*BindFramebuffer)(GLenum, GLuint);
    void (*BindTexture)(GLenum, GLuint);
    GLenum (*CheckFramebufferStatus)(GLenum);
    void (*Clear)(GLenum);
    void (*ClearColor)(GLfloat, GLfloat, GLfloat, GLfloat);
    void (*CompileShader)(GLuint);
    GLuint (*CreateProgram)(void);
    GLuint (*CreateShader)(GLenum);
    void (*DeleteFramebuffers)(GLsizei, const GLuint *);
    void (*DeleteProgram)(GLuint);
    void (*DeleteShader)(GLuint);
    void (*DeleteTextures)(GLsizei, const GLuint *);
    void (*DeleteVertexArrays)(GLsizei, const GLuint *);
    void (*Disable)(GLenum);
    void (*DrawArrays)(GLenum, GLint, GLsizei);
    void (*EGLImageTargetTexture2DOES)(GLenum, void *);
    void (*Enable)(GLenum);
    void (*Finish)(void);
    void (*FramebufferTexture2D)(GLenum, GLenum, GLenum, GLuint, GLint);
    void (*GenFramebuffers)(GLsizei, GLuint *);
    void (*GenTextures)(GLsizei, GLuint *);
    void (*GenVertexArrays)(GLsizei, GLuint *);
    GLenum (*GetError)(void);
    void (*GetProgramiv)(GLuint, GLenum, GLint *);
    void (*GetShaderiv)(GLuint, GLenum, GLint *);
    GLint (*GetUniformLocation)(GLuint, const GLchar *);
    void (*LinkProgram)(GLuint);
    void (*Scissor)(GLint, GLint, GLsizei, GLsizei);
    void (*ShaderSource)(GLuint, GLsizei, const GLchar *const *, const GLint *);
    void (*TexImage2D)(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *);
    void (*TexParameteri)(GLenum, GLenum, GLint);
    void (*Uniform1i)(GLint, GLint);
    void (*Uniform4f)(GLint, GLfloat, GLfloat, GLfloat, GLfloat);
    void (*UseProgram)(GLuint);
    void (*BindVertexArray)(GLuint);
    void (*Viewport)(GLint, GLint, GLsizei, GLsizei);
} Gles;

static int load_gles(Gles *gl) {
#define LOAD(name) if (!(*(void **)&gl->name = tp_egl_proc("gl" #name))) return 0;
    LOAD(ActiveTexture) LOAD(AttachShader) LOAD(BindFramebuffer) LOAD(BindTexture)
    LOAD(CheckFramebufferStatus) LOAD(Clear) LOAD(ClearColor) LOAD(CompileShader)
    LOAD(CreateProgram) LOAD(CreateShader) LOAD(DeleteFramebuffers) LOAD(DeleteProgram)
    LOAD(DeleteShader) LOAD(DeleteTextures) LOAD(DeleteVertexArrays) LOAD(Disable)
    LOAD(DrawArrays) LOAD(EGLImageTargetTexture2DOES) LOAD(Enable) LOAD(Finish)
    LOAD(FramebufferTexture2D) LOAD(GenFramebuffers) LOAD(GenTextures) LOAD(GenVertexArrays)
    LOAD(GetError) LOAD(GetProgramiv) LOAD(GetShaderiv) LOAD(GetUniformLocation)
    LOAD(LinkProgram) LOAD(Scissor) LOAD(ShaderSource) LOAD(TexImage2D) LOAD(TexParameteri)
    LOAD(Uniform1i) LOAD(Uniform4f) LOAD(UseProgram) LOAD(BindVertexArray) LOAD(Viewport)
#undef LOAD
    return 1;
}

/* A full-screen strip from gl_VertexID; `coord` spans the crop rectangle in
 * source texture coordinates. Output row 0 (the first row CUDA copies) is
 * drawn from source row 0, so there is no flip. */
static const char *VERTEX =
    "#version 300 es\n"
    "uniform vec4 crop;\n"
    "out vec2 coord;\n"
    "void main() {\n"
    "  vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));\n"
    "  coord = crop.xy + corner * crop.zw;\n"
    "  gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);\n"
    "}\n";

/* Limited-range BT.709, the matrix every other path (encoder colour tags,
 * client shader, sws) uses. Chroma is drawn at half size, so one bilinear
 * sample at each output texel centre averages a 2x2 source block. */
static const char *FRAGMENT =
    "#version 300 es\n"
    "#extension GL_OES_EGL_image_external_essl3 : require\n"
    "precision highp float;\n"
    "uniform samplerExternalOES source;\n"
    "uniform int chroma;\n"
    "in vec2 coord;\n"
    "out vec4 color;\n"
    "void main() {\n"
    "  vec3 rgb = texture(source, coord).rgb;\n"
    "  float y = dot(rgb, vec3(0.2126, 0.7152, 0.0722));\n"
    "  if (chroma == 0) {\n"
    "    color = vec4(16.0 / 255.0 + y * (219.0 / 255.0), 0.0, 0.0, 1.0);\n"
    "  } else {\n"
    "    vec2 c = vec2((rgb.b - y) / 1.8556, (rgb.r - y) / 1.5748);\n"
    "    color = vec4(128.0 / 255.0 + c * (224.0 / 255.0), 0.0, 1.0);\n"
    "  }\n"
    "}\n";

typedef struct {
    Egl *egl;
    Gles gl;
    EGLDisplay display;
    EGLContext context;
    GLuint program, vao, source, planes[2], framebuffers[2];
    GLint crop, chroma, sampler;
} GlesContext;

static void gles_release(GlesContext *g) {
    g->egl->BindAPI(EGL_OPENGL_ES_API);
    g->egl->MakeCurrent(g->display, NULL, NULL, NULL);
}

static int gles_acquire(GlesContext *g) {
    g->egl->BindAPI(EGL_OPENGL_ES_API);
    return g->egl->MakeCurrent(g->display, NULL, NULL, g->context) ? 0 : AVERROR_EXTERNAL;
}

static void gles_destroy(GlesContext *g) {
    if (!g->context) return;
    if (gles_acquire(g) == 0) {
        Gles *gl = &g->gl;
        if (g->framebuffers[0]) gl->DeleteFramebuffers(2, g->framebuffers);
        if (g->planes[0]) gl->DeleteTextures(2, g->planes);
        if (g->source) gl->DeleteTextures(1, &g->source);
        if (g->vao) gl->DeleteVertexArrays(1, &g->vao);
        if (g->program) gl->DeleteProgram(g->program);
        gles_release(g);
    }
    g->egl->DestroyContext(g->display, g->context);
    g->context = NULL;
}

/* Surfaceless GLES 3 context on `display`; current on return. */
static int gles_create(GlesContext *g, Egl *egl, EGLDisplay display) {
    memset(g, 0, sizeof *g);
    g->egl = egl;
    g->display = display;
    if (!load_gles(&g->gl)) return AVERROR(ENOSYS);
    if (!has_extension(egl->QueryString(display, EGL_EXTENSIONS), "EGL_KHR_surfaceless_context"))
        return AVERROR(ENOSYS);
    egl->BindAPI(EGL_OPENGL_ES_API);
    EGLint attributes[] = {EGL_SURFACE_TYPE, EGL_DONT_CARE, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE};
    EGLConfig config;
    EGLint configs = 0;
    if (!egl->ChooseConfig(display, attributes, &config, 1, &configs) || configs < 1) return AVERROR(ENOSYS);
    EGLint context_attributes[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE};
    g->context = egl->CreateContext(display, config, NULL, context_attributes);
    if (!g->context) return AVERROR(ENOSYS);
    return gles_acquire(g);
}

static GLuint compile(Gles *gl, GLenum type, const char *source) {
    GLuint id = gl->CreateShader(type);
    gl->ShaderSource(id, 1, &source, NULL);
    gl->CompileShader(id);
    GLint ok = 0;
    gl->GetShaderiv(id, GL_COMPILE_STATUS, &ok);
    if (!ok) { gl->DeleteShader(id); return 0; }
    return id;
}

/* Program, source texture and the two NV12 plane render targets (current
 * context). */
static int gles_converter(GlesContext *g, int width, int height) {
    Gles *gl = &g->gl;
    GLuint vs = compile(gl, GL_VERTEX_SHADER, VERTEX), fs = compile(gl, GL_FRAGMENT_SHADER, FRAGMENT);
    if (vs && fs) {
        g->program = gl->CreateProgram();
        gl->AttachShader(g->program, vs);
        gl->AttachShader(g->program, fs);
        gl->LinkProgram(g->program);
        GLint linked = 0;
        gl->GetProgramiv(g->program, GL_LINK_STATUS, &linked);
        if (!linked) { gl->DeleteProgram(g->program); g->program = 0; }
    }
    if (vs) gl->DeleteShader(vs);
    if (fs) gl->DeleteShader(fs);
    if (!g->program) return AVERROR(ENOSYS);
    g->crop = gl->GetUniformLocation(g->program, "crop");
    g->chroma = gl->GetUniformLocation(g->program, "chroma");
    g->sampler = gl->GetUniformLocation(g->program, "source");
    gl->GenVertexArrays(1, &g->vao);
    gl->GenTextures(1, &g->source);
    gl->BindTexture(GL_TEXTURE_EXTERNAL_OES, g->source);
    gl->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    gl->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    gl->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    gl->TexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    gl->GenTextures(2, g->planes);
    gl->GenFramebuffers(2, g->framebuffers);
    for (int i = 0; i < 2; i++) {
        gl->BindTexture(GL_TEXTURE_2D, g->planes[i]);
        gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        gl->TexImage2D(GL_TEXTURE_2D, 0, i ? GL_RG8 : GL_R8, i ? width / 2 : width,
            i ? height / 2 : height, 0, i ? GL_RG : GL_RED, GL_UNSIGNED_BYTE, NULL);
        gl->BindFramebuffer(GL_FRAMEBUFFER, g->framebuffers[i]);
        gl->FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, g->planes[i], 0);
        if (gl->CheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return AVERROR(ENOSYS);
    }
    gl->BindFramebuffer(GL_FRAMEBUFFER, 0);
    return gl->GetError() == GL_NO_ERROR ? 0 : AVERROR_EXTERNAL;
}

/* EGLImage for a DMA-BUF, with explicit modifiers. */
static EGLImage import_image(Egl *egl, EGLDisplay display, const TpDmabuf *source) {
    EGLint attributes[6 + 4 * 10 + 1];
    int n = 0;
    attributes[n++] = EGL_WIDTH; attributes[n++] = source->width;
    attributes[n++] = EGL_HEIGHT; attributes[n++] = source->height;
    attributes[n++] = EGL_LINUX_DRM_FOURCC_EXT; attributes[n++] = (EGLint)source->fourcc;
    for (int p = 0; p < source->planes; p++) {
        attributes[n++] = PLANE_ATTRIBUTES[p][0]; attributes[n++] = source->fds[p];
        attributes[n++] = PLANE_ATTRIBUTES[p][1]; attributes[n++] = (EGLint)source->offsets[p];
        attributes[n++] = PLANE_ATTRIBUTES[p][2]; attributes[n++] = (EGLint)source->pitches[p];
        if (source->modifier != DRM_FORMAT_MOD_INVALID) {
            attributes[n++] = PLANE_ATTRIBUTES[p][3]; attributes[n++] = (EGLint)(source->modifier & 0xffffffff);
            attributes[n++] = PLANE_ATTRIBUTES[p][4]; attributes[n++] = (EGLint)(source->modifier >> 32);
        }
    }
    attributes[n] = EGL_NONE;
    return egl->CreateImageKHR(display, NULL, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
}

/* ---- Importer ---- */

struct TpImport {
    Egl egl;
    enum AVHWDeviceType type;
    int width, height;
    /* VA-API */
    VADisplay va;
    VAConfigID va_config;
    VAContextID va_context;
    /* CUDA */
    CudaFunctions *cu;
    CUcontext cuda;
    GlesContext gles;
    CUgraphicsResource resources[2];
};

void tp_import_free(TpImport *import) {
    if (!import) return;
    if (import->va) {
        if (import->va_context != VA_INVALID_ID) vaDestroyContext(import->va, import->va_context);
        if (import->va_config != VA_INVALID_ID) vaDestroyConfig(import->va, import->va_config);
    }
    if (import->cu) {
        CUcontext ignored;
        int pushed = import->cuda && import->cu->cuCtxPushCurrent(import->cuda) == CUDA_SUCCESS;
        for (int i = 0; i < 2; i++)
            if (import->resources[i]) import->cu->cuGraphicsUnregisterResource(import->resources[i]);
        if (pushed) import->cu->cuCtxPopCurrent(&ignored);
        gles_destroy(&import->gles);
        cuda_free_functions(&import->cu);
    }
    free(import);
}

static int cuda_setup(TpImport *import, AVHWFramesContext *frames) {
    AVCUDADeviceContext *device = frames->device_ctx->hwctx;
    if (cuda_load_functions(&import->cu, NULL) != 0) return AVERROR(ENOSYS);
    CudaFunctions *cu = import->cu;
    import->cuda = device->cuda_ctx;
    CUdevice ordinal = -1;
    CUcontext ignored;
    if (cu->cuCtxPushCurrent(import->cuda) != CUDA_SUCCESS) return AVERROR_EXTERNAL;
    if (cu->cuCtxGetDevice(&ordinal) != CUDA_SUCCESS) ordinal = -1;
    cu->cuCtxPopCurrent(&ignored);
    EGLDisplay display = find_display(&import->egl, NULL, ordinal);
    if (!display) return AVERROR(ENODEV);
    int result = gles_create(&import->gles, &import->egl, display);
    if (result == 0) result = gles_converter(&import->gles, import->width, import->height);
    if (result == 0 && cu->cuCtxPushCurrent(import->cuda) == CUDA_SUCCESS) {
        for (int i = 0; i < 2 && result == 0; i++)
            if (cu->cuGraphicsGLRegisterImage(&import->resources[i], import->gles.planes[i], GL_TEXTURE_2D,
                    CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY) != CUDA_SUCCESS) result = AVERROR_EXTERNAL;
        cu->cuCtxPopCurrent(&ignored);
    } else if (result == 0) {
        result = AVERROR_EXTERNAL;
    }
    if (import->gles.context) gles_release(&import->gles);
    return result;
}

static int vaapi_setup(TpImport *import, AVHWFramesContext *frames) {
    AVVAAPIDeviceContext *device = frames->device_ctx->hwctx;
    import->va = device->display;
    if (vaCreateConfig(import->va, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &import->va_config)
        != VA_STATUS_SUCCESS) return AVERROR(ENOSYS);
    if (vaCreateContext(import->va, import->va_config, import->width, import->height, VA_PROGRESSIVE,
            NULL, 0, &import->va_context) != VA_STATUS_SUCCESS) return AVERROR(ENOSYS);
    return 0;
}

static int import_open(TpImport **out, AVBufferRef *frames_ref) {
    if (*out) return 0;
    if (!frames_ref) return AVERROR(EINVAL);
    AVHWFramesContext *frames = (AVHWFramesContext *)frames_ref->data;
    if (frames->sw_format != AV_PIX_FMT_NV12) return AVERROR(ENOSYS); /* HDR stays on the CPU path */
    TpImport *import = calloc(1, sizeof *import);
    if (!import) return AVERROR(ENOMEM);
    import->va_config = import->va_context = VA_INVALID_ID;
    import->type = frames->device_ctx->type;
    import->width = frames->width;
    import->height = frames->height;
    int result = load_egl(&import->egl) ? 0 : AVERROR(ENOSYS);
    if (result == 0)
        result = import->type == AV_HWDEVICE_TYPE_CUDA ? cuda_setup(import, frames)
            : import->type == AV_HWDEVICE_TYPE_VAAPI ? vaapi_setup(import, frames) : AVERROR(ENOSYS);
    if (result < 0) { tp_import_free(import); return result; }
    *out = import;
    return 0;
}

static int rgb_fourcc(uint32_t fourcc) {
    return fourcc == DRM_FORMAT_XRGB8888 || fourcc == DRM_FORMAT_ARGB8888
        || fourcc == DRM_FORMAT_XBGR8888 || fourcc == DRM_FORMAT_ABGR8888;
}

/* Intel compression (CCS) modifiers need auxiliary planes the VA drivers do
 * not reliably import for RGB; offer plain tiling only. */
static int vaapi_modifier(uint64_t modifier) {
    uint64_t vendor = modifier >> 56, code = modifier & 0x00ffffffffffffffULL;
    return vendor != 0x01 || code <= 3 || code == 9;
}

int tp_import_modifiers(TpImport **import, AVBufferRef *frames, const char *node, uint32_t fourcc,
    uint64_t *modifiers, int capacity) {
    if (!rgb_fourcc(fourcc) || !modifiers || capacity <= 0) return AVERROR(EINVAL);
    int result = import_open(import, frames);
    if (result < 0) return result;
    TpImport *g = *import;
    if (g->type == AV_HWDEVICE_TYPE_VAAPI && !node) return 0;
    EGLDisplay display = g->type == AV_HWDEVICE_TYPE_CUDA ? g->gles.display : find_display(&g->egl, node, -1);
    if (!display) return 0;
    EGLuint64KHR found[64];
    EGLBoolean external[64];
    EGLint count = 0;
    if (!g->egl.QueryDmaBufModifiersEXT(display, (EGLint)fourcc, 64, found, external, &count)) return 0;
    int n = 0;
    for (int i = 0; i < count && n < capacity; i++) {
        /* CUDA samples through an external texture, so external-only
         * layouts (NVIDIA linear) work too. */
        if (g->type == AV_HWDEVICE_TYPE_VAAPI && !vaapi_modifier(found[i])) continue;
        modifiers[n++] = found[i];
    }
    return n;
}

static uint32_t va_fourcc(uint32_t drm) {
    switch (drm) {
    case DRM_FORMAT_XRGB8888: return VA_FOURCC_BGRX;
    case DRM_FORMAT_ARGB8888: return VA_FOURCC_BGRA;
    case DRM_FORMAT_XBGR8888: return VA_FOURCC_RGBX;
    default: return VA_FOURCC_RGBA;
    }
}

static int convert_vaapi(TpImport *g, const TpDmabuf *source, AVFrame *output) {
    VADRMPRIMESurfaceDescriptor descriptor = {0};
    descriptor.fourcc = va_fourcc(source->fourcc);
    descriptor.width = source->width;
    descriptor.height = source->height;
    descriptor.num_layers = 1;
    descriptor.layers[0].drm_format = source->fourcc;
    descriptor.layers[0].num_planes = source->planes;
    for (int p = 0; p < source->planes; p++) {
        uint32_t object = 0;
        while (object < descriptor.num_objects && descriptor.objects[object].fd != source->fds[p]) object++;
        if (object == descriptor.num_objects) {
            errno = 0;
            off_t size = lseek(source->fds[p], 0, SEEK_END);
            if (size <= 0) return AVERROR(errno ? errno : EINVAL);
            descriptor.objects[object].fd = source->fds[p];
            descriptor.objects[object].size = (uint32_t)size;
            descriptor.objects[object].drm_format_modifier = source->modifier;
            descriptor.num_objects++;
        }
        descriptor.layers[0].object_index[p] = object;
        descriptor.layers[0].offset[p] = source->offsets[p];
        descriptor.layers[0].pitch[p] = source->pitches[p];
    }
    VASurfaceAttrib attributes[2] = {
        {.type = VASurfaceAttribMemoryType, .flags = VA_SURFACE_ATTRIB_SETTABLE,
         .value = {.type = VAGenericValueTypeInteger, .value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2}},
        {.type = VASurfaceAttribExternalBufferDescriptor, .flags = VA_SURFACE_ATTRIB_SETTABLE,
         .value = {.type = VAGenericValueTypePointer, .value.p = &descriptor}},
    };
    VASurfaceID imported;
    if (vaCreateSurfaces(g->va, VA_RT_FORMAT_RGB32, source->width, source->height, &imported, 1,
            attributes, 2) != VA_STATUS_SUCCESS) return AVERROR_EXTERNAL;
    VASurfaceID target = (VASurfaceID)(uintptr_t)output->data[3];
    VARectangle input = {source->crop_x, source->crop_y, source->crop_width, source->crop_height};
    VARectangle region = {0, 0, g->width, g->height};
    VAProcPipelineParameterBuffer parameters = {0};
    parameters.surface = imported;
    parameters.surface_region = &input;
    parameters.output_region = &region;
    parameters.output_background_color = 0xff000000;
    parameters.surface_color_standard = VAProcColorStandardBT709;
    parameters.output_color_standard = VAProcColorStandardBT709;
    parameters.input_color_properties.color_range = VA_SOURCE_RANGE_FULL;
    parameters.output_color_properties.color_range = VA_SOURCE_RANGE_REDUCED;
    parameters.filter_flags = VA_FILTER_SCALING_DEFAULT;
    VABufferID buffer;
    int result = AVERROR_EXTERNAL;
    if (vaCreateBuffer(g->va, g->va_context, VAProcPipelineParameterBufferType, sizeof parameters, 1,
            &parameters, &buffer) == VA_STATUS_SUCCESS) {
        if (vaBeginPicture(g->va, g->va_context, target) == VA_STATUS_SUCCESS) {
            VAStatus render = vaRenderPicture(g->va, g->va_context, &buffer, 1);
            /* End even after a failed render: it closes the picture. */
            if (vaEndPicture(g->va, g->va_context) == VA_STATUS_SUCCESS && render == VA_STATUS_SUCCESS)
                result = 0;
        }
        vaDestroyBuffer(g->va, buffer);
    }
    /* The compositor may reuse its buffer as soon as we return. */
    if (vaSyncSurface(g->va, target) != VA_STATUS_SUCCESS) result = AVERROR_EXTERNAL;
    vaDestroySurfaces(g->va, &imported, 1);
    return result;
}

static int convert_cuda(TpImport *g, const TpDmabuf *source, AVFrame *output) {
    GlesContext *c = &g->gles;
    Gles *gl = &c->gl;
    if (gles_acquire(c) < 0) return AVERROR_EXTERNAL;
    int result = 0;
    EGLImage image = import_image(&g->egl, c->display, source);
    if (!image) result = AVERROR_EXTERNAL;
    if (result == 0) {
        gl->ActiveTexture(GL_TEXTURE0);
        gl->BindTexture(GL_TEXTURE_EXTERNAL_OES, c->source);
        gl->EGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES, image);
        gl->UseProgram(c->program);
        gl->BindVertexArray(c->vao);
        gl->Uniform1i(c->sampler, 0);
        gl->Uniform4f(c->crop, (GLfloat)source->crop_x / source->width, (GLfloat)source->crop_y / source->height,
            (GLfloat)source->crop_width / source->width, (GLfloat)source->crop_height / source->height);
        for (int plane = 0; plane < 2; plane++) {
            gl->BindFramebuffer(GL_FRAMEBUFFER, c->framebuffers[plane]);
            gl->Viewport(0, 0, plane ? g->width / 2 : g->width, plane ? g->height / 2 : g->height);
            gl->Uniform1i(c->chroma, plane);
            gl->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
        gl->BindFramebuffer(GL_FRAMEBUFFER, 0);
        if (gl->GetError() != GL_NO_ERROR) result = AVERROR_EXTERNAL;
    }
    CudaFunctions *cu = g->cu;
    CUcontext ignored;
    if (result == 0 && cu->cuCtxPushCurrent(g->cuda) != CUDA_SUCCESS) result = AVERROR_EXTERNAL;
    else if (result == 0) {
        /* Mapping waits for the draws above; the synchronous copies finish
         * before unmapping, after which the source buffer is free. */
        int mapped = cu->cuGraphicsMapResources(2, g->resources, 0) == CUDA_SUCCESS;
        if (!mapped) result = AVERROR_EXTERNAL;
        for (int plane = 0; plane < 2 && result == 0; plane++) {
            CUarray array;
            if (cu->cuGraphicsSubResourceGetMappedArray(&array, g->resources[plane], 0, 0) != CUDA_SUCCESS) {
                result = AVERROR_EXTERNAL;
                break;
            }
            CUDA_MEMCPY2D copy = {0};
            copy.srcMemoryType = CU_MEMORYTYPE_ARRAY;
            copy.srcArray = array;
            copy.dstMemoryType = CU_MEMORYTYPE_DEVICE;
            copy.dstDevice = (CUdeviceptr)output->data[plane];
            copy.dstPitch = output->linesize[plane];
            copy.WidthInBytes = g->width; /* chroma: width/2 texels of 2 bytes */
            copy.Height = plane ? g->height / 2 : g->height;
            if (cu->cuMemcpy2D(&copy) != CUDA_SUCCESS) result = AVERROR_EXTERNAL;
        }
        if (mapped) cu->cuGraphicsUnmapResources(2, g->resources, 0);
        cu->cuCtxPopCurrent(&ignored);
    }
    if (image) {
        /* Detach the compositor's buffer before dropping our image. */
        g->egl.DestroyImageKHR(c->display, image);
    }
    gles_release(c);
    return result;
}

int tp_import_convert(TpImport **import, AVBufferRef *frames, const char *node,
    const TpDmabuf *source, AVFrame *output) {
    (void)node;
    if (!source || !output || !rgb_fourcc(source->fourcc) || source->planes < 1 || source->planes > 4
        || source->width < 2 || source->height < 2 || source->crop_x < 0 || source->crop_y < 0
        || source->crop_width < 2 || source->crop_height < 2
        || source->crop_x + source->crop_width > source->width
        || source->crop_y + source->crop_height > source->height) return AVERROR(EINVAL);
    for (int p = 0; p < source->planes; p++)
        if (source->fds[p] < 0 || source->pitches[p] == 0) return AVERROR(EINVAL);
    int result = import_open(import, frames);
    if (result < 0) return result;
    av_frame_unref(output);
    result = av_hwframe_get_buffer(frames, output, 0);
    if (result < 0) return result;
    result = (*import)->type == AV_HWDEVICE_TYPE_CUDA ? convert_cuda(*import, source, output)
        : convert_vaapi(*import, source, output);
    if (result < 0) av_frame_unref(output);
    return result;
}

/* ---- Test source ---- */

struct TpTestDmabuf {
    Egl egl;
    GlesContext gles;
    GLuint texture;
    EGLImage image;
    TpDmabuf dmabuf;
};

void tp_test_dmabuf_free(TpTestDmabuf *source) {
    if (!source) return;
    for (int p = 0; p < source->dmabuf.planes; p++)
        if (source->dmabuf.fds[p] >= 0) close(source->dmabuf.fds[p]);
    if (source->image) source->egl.DestroyImageKHR(source->gles.display, source->image);
    if (source->texture && gles_acquire(&source->gles) == 0) {
        source->gles.gl.DeleteTextures(1, &source->texture);
        gles_release(&source->gles);
    }
    gles_destroy(&source->gles);
    free(source);
}

TpTestDmabuf *tp_test_dmabuf_new(const char *node, int width, int height, const uint8_t *colors,
    TpDmabuf *out, int *error) {
    *error = AVERROR(ENOMEM);
    TpTestDmabuf *s = calloc(1, sizeof *s);
    if (!s) return NULL;
    for (int p = 0; p < 4; p++) s->dmabuf.fds[p] = -1;
    *error = AVERROR(ENODEV);
    if (!load_egl(&s->egl) || !s->egl.ExportDMABUFImageQueryMESA || !s->egl.ExportDMABUFImageMESA) goto fail;
    EGLDisplay display = find_display(&s->egl, node, -1);
    if (!display || !has_extension(s->egl.QueryString(display, EGL_EXTENSIONS), "EGL_MESA_image_dma_buf_export"))
        goto fail;
    *error = gles_create(&s->gles, &s->egl, display);
    if (*error < 0) goto fail;
    Gles *gl = &s->gles.gl;
    GLuint framebuffer;
    gl->GenTextures(1, &s->texture);
    gl->BindTexture(GL_TEXTURE_2D, s->texture);
    gl->TexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, width, height, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    gl->GenFramebuffers(1, &framebuffer);
    gl->BindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    gl->FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, s->texture, 0);
    gl->Enable(GL_SCISSOR_TEST);
    /* Texture row 0 is the buffer's first row: the image's top. */
    for (int q = 0; q < 4; q++) {
        int right = q & 1, bottom = q >> 1;
        gl->Scissor(right ? width / 2 : 0, bottom ? height / 2 : 0, right ? width - width / 2 : width / 2,
            bottom ? height - height / 2 : height / 2);
        gl->ClearColor(colors[q * 3] / 255.0f, colors[q * 3 + 1] / 255.0f, colors[q * 3 + 2] / 255.0f, 1.0f);
        gl->Clear(GL_COLOR_BUFFER_BIT);
    }
    gl->Disable(GL_SCISSOR_TEST);
    gl->BindFramebuffer(GL_FRAMEBUFFER, 0);
    gl->DeleteFramebuffers(1, &framebuffer);
    gl->Finish();
    *error = AVERROR_EXTERNAL;
    s->image = s->egl.CreateImageKHR(display, s->gles.context, EGL_GL_TEXTURE_2D,
        (void *)(uintptr_t)s->texture, (EGLint[]){EGL_NONE});
    int fourcc = 0, planes = 0;
    EGLuint64KHR modifier = 0;
    EGLint strides[4] = {0}, offsets[4] = {0};
    if (!s->image || gl->GetError() != GL_NO_ERROR
        || !s->egl.ExportDMABUFImageQueryMESA(display, s->image, &fourcc, &planes, &modifier)
        || planes < 1 || planes > 4
        || !s->egl.ExportDMABUFImageMESA(display, s->image, s->dmabuf.fds, strides, offsets)) {
        gles_release(&s->gles);
        goto fail;
    }
    gles_release(&s->gles);
    s->dmabuf.planes = planes;
    s->dmabuf.fourcc = (uint32_t)fourcc;
    s->dmabuf.modifier = modifier;
    s->dmabuf.width = s->dmabuf.crop_width = width;
    s->dmabuf.height = s->dmabuf.crop_height = height;
    for (int p = 0; p < planes; p++) {
        s->dmabuf.offsets[p] = (uint32_t)offsets[p];
        s->dmabuf.pitches[p] = (uint32_t)strides[p];
    }
    *out = s->dmabuf;
    *error = 0;
    return s;
fail:
    tp_test_dmabuf_free(s);
    return NULL;
}

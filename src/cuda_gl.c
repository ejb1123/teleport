/* Zero-copy presentation of NVDEC (FFmpeg CUDA) frames in an OpenGL context.
 *
 * The decoded NV12 surface never leaves the GPU: CUDA copies its two planes
 * device-to-device into GL textures registered for interop, and a small
 * shader converts limited-range BT.709 NV12 to RGB while drawing a quad into
 * the current framebuffer. Replaces download + CPU copies + texture upload
 * (measured 0.67 -> 0.22 ms at 1080p, 2.78 -> 0.27 ms at 4K for the transfer
 * alone on an RTX 3080).
 *
 * GL entry points come from a caller-supplied resolver (SDL's in the app,
 * EGL's in the self-test) and libcuda is loaded dynamically by FFmpeg's
 * nv-codec-headers loader, so nothing new is linked. All GL state touched is
 * saved and restored: the host renderer (SDL) caches its own state.
 */
#include <ffnvcodec/dynlink_loader.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_cuda.h>
#include <libavutil/pixdesc.h>
#include <dlfcn.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

typedef unsigned int GLenum;
typedef unsigned int GLuint;
typedef int GLint;
typedef int GLsizei;
typedef unsigned char GLboolean;
typedef char GLchar;
typedef float GLfloat;
typedef ptrdiff_t GLsizeiptr;
typedef void GLvoid;

#define GL_TEXTURE_2D 0x0DE1
#define GL_TEXTURE0 0x84C0
#define GL_TEXTURE1 0x84C1
#define GL_ACTIVE_TEXTURE 0x84E0
#define GL_TEXTURE_BINDING_2D 0x8069
#define GL_TEXTURE_MIN_FILTER 0x2801
#define GL_TEXTURE_MAG_FILTER 0x2800
#define GL_TEXTURE_WRAP_S 0x2802
#define GL_TEXTURE_WRAP_T 0x2803
#define GL_LINEAR 0x2601
#define GL_CLAMP_TO_EDGE 0x812F
#define GL_R8 0x8229
#define GL_RG8 0x822B
#define GL_RED 0x1903
#define GL_RG 0x8227
#define GL_RGBA 0x1908
#define GL_RGBA8 0x8058
#define GL_UNSIGNED_BYTE 0x1401
#define GL_FLOAT 0x1406
#define GL_FALSE 0
#define GL_VERTEX_SHADER 0x8B31
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_CURRENT_PROGRAM 0x8B8D
#define GL_ARRAY_BUFFER 0x8892
#define GL_ARRAY_BUFFER_BINDING 0x8894
#define GL_STATIC_DRAW 0x88E4
#define GL_VIEWPORT 0x0BA2
#define GL_BLEND 0x0BE2
#define GL_SCISSOR_TEST 0x0C11
#define GL_DEPTH_TEST 0x0B71
#define GL_CULL_FACE 0x0B44
#define GL_TRIANGLE_STRIP 0x0005
#define GL_VERTEX_ATTRIB_ARRAY_ENABLED 0x8622
#define GL_VERTEX_ATTRIB_ARRAY_SIZE 0x8623
#define GL_VERTEX_ATTRIB_ARRAY_STRIDE 0x8624
#define GL_VERTEX_ATTRIB_ARRAY_TYPE 0x8625
#define GL_VERTEX_ATTRIB_ARRAY_NORMALIZED 0x886A
#define GL_VERTEX_ATTRIB_ARRAY_POINTER 0x8645
#define GL_VERTEX_ATTRIB_ARRAY_BUFFER_BINDING 0x889F
#define GL_FRAMEBUFFER 0x8D40
#define GL_FRAMEBUFFER_BINDING 0x8CA6
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5
#define GL_UNPACK_ALIGNMENT 0x0CF5
#define GL_PACK_ALIGNMENT 0x0D05
#define GL_NO_ERROR 0

typedef void *(*TpGetProc)(const char *name);

typedef struct {
    void (*ActiveTexture)(GLenum);
    void (*AttachShader)(GLuint, GLuint);
    void (*BindAttribLocation)(GLuint, GLuint, const GLchar *);
    void (*BindBuffer)(GLenum, GLuint);
    void (*BindTexture)(GLenum, GLuint);
    void (*BufferData)(GLenum, GLsizeiptr, const void *, GLenum);
    void (*CompileShader)(GLuint);
    GLuint (*CreateProgram)(void);
    GLuint (*CreateShader)(GLenum);
    void (*DeleteBuffers)(GLsizei, const GLuint *);
    void (*DeleteProgram)(GLuint);
    void (*DeleteShader)(GLuint);
    void (*DeleteTextures)(GLsizei, const GLuint *);
    void (*Disable)(GLenum);
    void (*DisableVertexAttribArray)(GLuint);
    void (*DrawArrays)(GLenum, GLint, GLsizei);
    void (*Enable)(GLenum);
    void (*EnableVertexAttribArray)(GLuint);
    void (*GenBuffers)(GLsizei, GLuint *);
    void (*GenTextures)(GLsizei, GLuint *);
    GLenum (*GetError)(void);
    void (*GetIntegerv)(GLenum, GLint *);
    void (*GetProgramiv)(GLuint, GLenum, GLint *);
    void (*GetShaderiv)(GLuint, GLenum, GLint *);
    GLint (*GetUniformLocation)(GLuint, const GLchar *);
    void (*GetVertexAttribiv)(GLuint, GLenum, GLint *);
    void (*GetVertexAttribPointerv)(GLuint, GLenum, void **);
    GLboolean (*IsEnabled)(GLenum);
    void (*LinkProgram)(GLuint);
    void (*ShaderSource)(GLuint, GLsizei, const GLchar *const *, const GLint *);
    void (*TexImage2D)(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *);
    void (*TexParameteri)(GLenum, GLenum, GLint);
    void (*Uniform1i)(GLint, GLint);
    void (*UseProgram)(GLuint);
    void (*VertexAttribPointer)(GLuint, GLint, GLenum, GLboolean, GLsizei, const void *);
    void (*Viewport)(GLint, GLint, GLsizei, GLsizei);
} Gl;

typedef struct TpCudaGl {
    Gl gl;
    CudaFunctions *cu;
    GLuint program, vbo, textures[2];
    GLint y_plane, uv_plane;
    int width, height;
    CUcontext registered_for;
    CUgraphicsResource resources[2];
} TpCudaGl;

static int load_gl(Gl *gl, TpGetProc get) {
#define LOAD(name) if (!(*(void **)&gl->name = get("gl" #name))) return 0;
    LOAD(ActiveTexture) LOAD(AttachShader) LOAD(BindAttribLocation) LOAD(BindBuffer)
    LOAD(BindTexture) LOAD(BufferData) LOAD(CompileShader) LOAD(CreateProgram)
    LOAD(CreateShader) LOAD(DeleteBuffers) LOAD(DeleteProgram) LOAD(DeleteShader)
    LOAD(DeleteTextures) LOAD(Disable) LOAD(DisableVertexAttribArray) LOAD(DrawArrays)
    LOAD(Enable) LOAD(EnableVertexAttribArray) LOAD(GenBuffers) LOAD(GenTextures)
    LOAD(GetError) LOAD(GetIntegerv) LOAD(GetProgramiv) LOAD(GetShaderiv)
    LOAD(GetUniformLocation) LOAD(GetVertexAttribiv) LOAD(GetVertexAttribPointerv)
    LOAD(IsEnabled) LOAD(LinkProgram) LOAD(ShaderSource) LOAD(TexImage2D)
    LOAD(TexParameteri) LOAD(Uniform1i) LOAD(UseProgram) LOAD(VertexAttribPointer)
    LOAD(Viewport)
#undef LOAD
    return 1;
}

static const char *VERTEX =
    "#version 120\n"
    "attribute vec2 position;\n"
    "varying vec2 coord;\n"
    "void main() {\n"
    "  coord = vec2(position.x * 0.5 + 0.5, 0.5 - position.y * 0.5);\n"
    "  gl_Position = vec4(position, 0.0, 1.0);\n"
    "}\n";

/* Limited-range BT.709, matching the encoder's colour negotiation. */
static const char *FRAGMENT =
    "#version 120\n"
    "uniform sampler2D y_plane;\n"
    "uniform sampler2D uv_plane;\n"
    "varying vec2 coord;\n"
    "void main() {\n"
    "  float y = (texture2D(y_plane, coord).r - 16.0 / 255.0) * (255.0 / 219.0);\n"
    "  vec2 c = (texture2D(uv_plane, coord).rg - 128.0 / 255.0) * (255.0 / 224.0);\n"
    "  vec3 rgb = vec3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);\n"
    "  gl_FragColor = vec4(clamp(rgb, 0.0, 1.0), 1.0);\n"
    "}\n";

static GLuint shader(Gl *gl, GLenum type, const char *source) {
    GLuint id = gl->CreateShader(type);
    gl->ShaderSource(id, 1, &source, NULL);
    gl->CompileShader(id);
    GLint ok = 0;
    gl->GetShaderiv(id, GL_COMPILE_STATUS, &ok);
    if (!ok) { gl->DeleteShader(id); return 0; }
    return id;
}

void tp_cuda_gl_free(TpCudaGl *g) {
    if (!g) return;
    if (g->cu) {
        for (int i = 0; i < 2; i++)
            if (g->resources[i]) g->cu->cuGraphicsUnregisterResource(g->resources[i]);
        cuda_free_functions(&g->cu);
    }
    if (g->gl.DeleteTextures) {
        if (g->textures[0]) g->gl.DeleteTextures(2, g->textures);
        if (g->vbo) g->gl.DeleteBuffers(1, &g->vbo);
        if (g->program) g->gl.DeleteProgram(g->program);
    }
    free(g);
}

/* Create a presenter in the current GL context. NULL if GL 2.1 shaders or
 * libcuda are unavailable; the caller keeps its CPU presentation path. */
TpCudaGl *tp_cuda_gl_new(TpGetProc get) {
    TpCudaGl *g = calloc(1, sizeof *g);
    if (!g || !get || !load_gl(&g->gl, get) || cuda_load_functions(&g->cu, NULL) != 0) {
        tp_cuda_gl_free(g);
        return NULL;
    }
    /* CUDA can only register textures of an NVIDIA GL context (not Mesa,
     * llvmpipe or another vendor's GPU in a hybrid system). */
    const unsigned char *(*get_string)(GLenum) = (const unsigned char *(*)(GLenum))get("glGetString");
    const char *vendor = get_string ? (const char *)get_string(0x1F00 /* GL_VENDOR */) : NULL;
    if (!vendor || !strstr(vendor, "NVIDIA")) {
        tp_cuda_gl_free(g);
        return NULL;
    }
    Gl *gl = &g->gl;
    GLint saved_program = 0, saved_buffer = 0;
    gl->GetIntegerv(GL_CURRENT_PROGRAM, &saved_program);
    gl->GetIntegerv(GL_ARRAY_BUFFER_BINDING, &saved_buffer);
    GLuint vs = shader(gl, GL_VERTEX_SHADER, VERTEX), fs = shader(gl, GL_FRAGMENT_SHADER, FRAGMENT);
    if (vs && fs) {
        g->program = gl->CreateProgram();
        gl->AttachShader(g->program, vs);
        gl->AttachShader(g->program, fs);
        gl->BindAttribLocation(g->program, 0, "position");
        gl->LinkProgram(g->program);
        GLint linked = 0;
        gl->GetProgramiv(g->program, GL_LINK_STATUS, &linked);
        if (!linked) { gl->DeleteProgram(g->program); g->program = 0; }
    }
    if (vs) gl->DeleteShader(vs);
    if (fs) gl->DeleteShader(fs);
    if (g->program) {
        g->y_plane = gl->GetUniformLocation(g->program, "y_plane");
        g->uv_plane = gl->GetUniformLocation(g->program, "uv_plane");
        static const GLfloat quad[] = {-1, -1, 1, -1, -1, 1, 1, 1};
        gl->GenBuffers(1, &g->vbo);
        gl->BindBuffer(GL_ARRAY_BUFFER, g->vbo);
        gl->BufferData(GL_ARRAY_BUFFER, sizeof quad, quad, GL_STATIC_DRAW);
    }
    gl->BindBuffer(GL_ARRAY_BUFFER, (GLuint)saved_buffer);
    gl->UseProgram((GLuint)saved_program);
    if (!g->program || gl->GetError() != GL_NO_ERROR) {
        tp_cuda_gl_free(g);
        return NULL;
    }
    return g;
}

static int ensure_textures(TpCudaGl *g, CUcontext ctx, int width, int height) {
    if (g->width == width && g->height == height && g->registered_for == ctx) return 0;
    Gl *gl = &g->gl;
    for (int i = 0; i < 2; i++) {
        if (g->resources[i]) { g->cu->cuGraphicsUnregisterResource(g->resources[i]); g->resources[i] = NULL; }
    }
    if (g->textures[0]) gl->DeleteTextures(2, g->textures);
    gl->GenTextures(2, g->textures);
    for (int i = 0; i < 2; i++) {
        gl->BindTexture(GL_TEXTURE_2D, g->textures[i]);
        gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        gl->TexImage2D(GL_TEXTURE_2D, 0, i ? GL_RG8 : GL_R8, i ? width / 2 : width,
            i ? height / 2 : height, 0, i ? GL_RG : GL_RED, GL_UNSIGNED_BYTE, NULL);
    }
    if (gl->GetError() != GL_NO_ERROR) return AVERROR_EXTERNAL;
    for (int i = 0; i < 2; i++) {
        if (g->cu->cuGraphicsGLRegisterImage(&g->resources[i], g->textures[i], GL_TEXTURE_2D,
                CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD) != CUDA_SUCCESS)
            return AVERROR_EXTERNAL;
    }
    g->width = width; g->height = height; g->registered_for = ctx;
    return 0;
}

/* Copy the frame's planes into the interop textures (GPU to GPU). */
static int upload(TpCudaGl *g, const AVFrame *frame) {
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
    if (!g || !frame || width <= 0 || height <= 0) return AVERROR(EINVAL);
    Gl *gl = &g->gl;
    GLint program, buffer, active, viewport[4], bound[2];
    gl->GetIntegerv(GL_CURRENT_PROGRAM, &program);
    gl->GetIntegerv(GL_ARRAY_BUFFER_BINDING, &buffer);
    gl->GetIntegerv(GL_ACTIVE_TEXTURE, &active);
    gl->GetIntegerv(GL_VIEWPORT, viewport);
    for (int i = 0; i < 2; i++) {
        gl->ActiveTexture(GL_TEXTURE0 + i);
        gl->GetIntegerv(GL_TEXTURE_BINDING_2D, &bound[i]);
    }
    GLboolean blend = gl->IsEnabled(GL_BLEND), scissor = gl->IsEnabled(GL_SCISSOR_TEST);
    GLboolean depth = gl->IsEnabled(GL_DEPTH_TEST), cull = gl->IsEnabled(GL_CULL_FACE);
    GLint attrib_enabled, attrib_size, attrib_type, attrib_normalized, attrib_stride, attrib_buffer;
    void *attrib_pointer;
    gl->GetVertexAttribiv(0, GL_VERTEX_ATTRIB_ARRAY_ENABLED, &attrib_enabled);
    gl->GetVertexAttribiv(0, GL_VERTEX_ATTRIB_ARRAY_SIZE, &attrib_size);
    gl->GetVertexAttribiv(0, GL_VERTEX_ATTRIB_ARRAY_TYPE, &attrib_type);
    gl->GetVertexAttribiv(0, GL_VERTEX_ATTRIB_ARRAY_NORMALIZED, &attrib_normalized);
    gl->GetVertexAttribiv(0, GL_VERTEX_ATTRIB_ARRAY_STRIDE, &attrib_stride);
    gl->GetVertexAttribiv(0, GL_VERTEX_ATTRIB_ARRAY_BUFFER_BINDING, &attrib_buffer);
    gl->GetVertexAttribPointerv(0, GL_VERTEX_ATTRIB_ARRAY_POINTER, &attrib_pointer);

    int result = upload(g, frame);
    if (result == 0) {
        gl->UseProgram(g->program);
        for (int i = 0; i < 2; i++) {
            gl->ActiveTexture(GL_TEXTURE0 + i);
            gl->BindTexture(GL_TEXTURE_2D, g->textures[i]);
        }
        gl->Uniform1i(g->y_plane, 0);
        gl->Uniform1i(g->uv_plane, 1);
        gl->Disable(GL_BLEND); gl->Disable(GL_SCISSOR_TEST);
        gl->Disable(GL_DEPTH_TEST); gl->Disable(GL_CULL_FACE);
        gl->Viewport(x, framebuffer_height - y - height, width, height);
        gl->BindBuffer(GL_ARRAY_BUFFER, g->vbo);
        gl->EnableVertexAttribArray(0);
        gl->VertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        gl->DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        if (gl->GetError() != GL_NO_ERROR) result = AVERROR_EXTERNAL;
    }

    /* Restore exactly what the host renderer had. */
    gl->BindBuffer(GL_ARRAY_BUFFER, (GLuint)attrib_buffer);
    gl->VertexAttribPointer(0, attrib_size, (GLenum)attrib_type, (GLboolean)attrib_normalized,
        attrib_stride, attrib_pointer);
    if (attrib_enabled) gl->EnableVertexAttribArray(0); else gl->DisableVertexAttribArray(0);
    gl->BindBuffer(GL_ARRAY_BUFFER, (GLuint)buffer);
    if (blend) gl->Enable(GL_BLEND);
    if (scissor) gl->Enable(GL_SCISSOR_TEST);
    if (depth) gl->Enable(GL_DEPTH_TEST);
    if (cull) gl->Enable(GL_CULL_FACE);
    gl->Viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
    for (int i = 1; i >= 0; i--) {
        gl->ActiveTexture(GL_TEXTURE0 + i);
        gl->BindTexture(GL_TEXTURE_2D, (GLuint)bound[i]);
    }
    gl->ActiveTexture((GLenum)active);
    gl->UseProgram((GLuint)program);
    return result;
}

/* ---- Offscreen self-test (no window, no linked EGL): used by cargo tests. ---- */

typedef void *EGLDisplay;
typedef void *EGLContext;
typedef void *EGLConfig;
typedef void *EGLDeviceEXT;
typedef int EGLint;
typedef unsigned int EGLBoolean;
#define EGL_NONE 0x3038
#define EGL_SURFACE_TYPE 0x3033
#define EGL_PBUFFER_BIT 0x0001
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_BIT 0x0008
#define EGL_OPENGL_API 0x30A2
#define EGL_VENDOR 0x3053
#define EGL_PLATFORM_DEVICE_EXT 0x313F

static void *egl_library;
static void *(*egl_get_proc)(const char *);
static void *self_test_proc(const char *name) {
    void *symbol = egl_get_proc ? egl_get_proc(name) : NULL;
    return symbol ? symbol : (egl_library ? dlsym(egl_library, name) : NULL);
}

/* Draw a CUDA frame into an offscreen RGBA framebuffer on an NVIDIA EGL
 * device and read it back. Returns 0 and fills `rgba` (width*height*4), a
 * positive AVERROR(ENODEV) when no NVIDIA EGL device exists, or an error. */
int tp_cuda_gl_self_test(const AVFrame *frame, uint8_t *rgba) {
    if (!frame) return AVERROR(EINVAL);
    if (!egl_library) egl_library = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!egl_library) return AVERROR(ENODEV);
    egl_get_proc = (void *(*)(const char *))dlsym(egl_library, "eglGetProcAddress");
    EGLBoolean (*query)(EGLint, EGLDeviceEXT *, EGLint *) = self_test_proc("eglQueryDevicesEXT");
    EGLDisplay (*platform)(EGLint, void *, const EGLint *) = self_test_proc("eglGetPlatformDisplayEXT");
    EGLBoolean (*initialize)(EGLDisplay, EGLint *, EGLint *) = self_test_proc("eglInitialize");
    const char *(*query_string)(EGLDisplay, EGLint) = self_test_proc("eglQueryString");
    EGLBoolean (*bind_api)(unsigned) = self_test_proc("eglBindAPI");
    EGLBoolean (*choose)(EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *) = self_test_proc("eglChooseConfig");
    EGLContext (*create)(EGLDisplay, EGLConfig, EGLContext, const EGLint *) = self_test_proc("eglCreateContext");
    EGLBoolean (*make_current)(EGLDisplay, void *, void *, EGLContext) = self_test_proc("eglMakeCurrent");
    EGLBoolean (*destroy)(EGLDisplay, EGLContext) = self_test_proc("eglDestroyContext");
    if (!query || !platform || !initialize || !query_string || !bind_api || !choose || !create
        || !make_current || !destroy) return AVERROR(ENODEV);
    EGLDeviceEXT devices[8];
    EGLint count = 0;
    if (!query(8, devices, &count)) return AVERROR(ENODEV);
    for (int i = 0; i < count; i++) {
        EGLDisplay display = platform(EGL_PLATFORM_DEVICE_EXT, devices[i], NULL);
        EGLint major, minor;
        if (!display || !initialize(display, &major, &minor)) continue;
        const char *vendor = query_string(display, EGL_VENDOR);
        if (!vendor || !strstr(vendor, "NVIDIA")) continue;
        bind_api(EGL_OPENGL_API);
        EGLint attributes[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
        EGLConfig config;
        EGLint configs = 0;
        if (!choose(display, attributes, &config, 1, &configs) || configs < 1) continue;
        EGLContext context = create(display, config, NULL, NULL);
        if (!context || !make_current(display, NULL, NULL, context)) continue;

        void (*gen_framebuffers)(GLsizei, GLuint *) = self_test_proc("glGenFramebuffers");
        void (*bind_framebuffer)(GLenum, GLuint) = self_test_proc("glBindFramebuffer");
        void (*framebuffer_texture)(GLenum, GLenum, GLenum, GLuint, GLint) = self_test_proc("glFramebufferTexture2D");
        GLenum (*status)(GLenum) = self_test_proc("glCheckFramebufferStatus");
        void (*read_pixels)(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, void *) = self_test_proc("glReadPixels");
        void (*pixel_store)(GLenum, GLint) = self_test_proc("glPixelStorei");
        void (*gen_textures)(GLsizei, GLuint *) = self_test_proc("glGenTextures");
        void (*bind_texture)(GLenum, GLuint) = self_test_proc("glBindTexture");
        void (*tex_image)(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *) = self_test_proc("glTexImage2D");
        int result = AVERROR(ENODEV);
        if (gen_framebuffers && bind_framebuffer && framebuffer_texture && status && read_pixels
            && pixel_store && gen_textures && bind_texture && tex_image) {
            GLuint target, framebuffer;
            gen_textures(1, &target);
            bind_texture(GL_TEXTURE_2D, target);
            tex_image(GL_TEXTURE_2D, 0, GL_RGBA8, frame->width, frame->height, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
            gen_framebuffers(1, &framebuffer);
            bind_framebuffer(GL_FRAMEBUFFER, framebuffer);
            framebuffer_texture(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target, 0);
            result = AVERROR_EXTERNAL;
            if (status(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE) {
                TpCudaGl *g = tp_cuda_gl_new(self_test_proc);
                if (g) {
                    result = tp_cuda_gl_draw(g, frame, 0, 0, frame->width, frame->height, frame->height);
                    if (result == 0) {
                        pixel_store(GL_PACK_ALIGNMENT, 1);
                        read_pixels(0, 0, frame->width, frame->height, GL_RGBA, GL_UNSIGNED_BYTE, rgba);
                    }
                    tp_cuda_gl_free(g);
                }
            }
        }
        make_current(display, NULL, NULL, NULL);
        destroy(display, context);
        return result;
    }
    return AVERROR(ENODEV);
}

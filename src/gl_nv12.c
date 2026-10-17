/* Shared NV12 OpenGL drawing for the zero-copy presenters; see gl_nv12.h. */
#include "gl_nv12.h"
#include <libavutil/error.h>
#include <dlfcn.h>
#include <errno.h>
#include <string.h>

#define GL_ACTIVE_TEXTURE 0x84E0
#define GL_TEXTURE_BINDING_2D 0x8069
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
#define GL_RGBA 0x1908
#define GL_RGBA8 0x8058
#define GL_FRAMEBUFFER 0x8D40
#define GL_FRAMEBUFFER_BINDING 0x8CA6
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5
#define GL_PACK_ALIGNMENT 0x0D05

static int load_gl(TpGl *gl, TpGetProc get) {
#define LOAD(name) if (!(*(void **)&gl->name = get("gl" #name))) return 0;
    LOAD(ActiveTexture) LOAD(AttachShader) LOAD(BindAttribLocation) LOAD(BindBuffer)
    LOAD(BindTexture) LOAD(BufferData) LOAD(CompileShader) LOAD(CreateProgram)
    LOAD(CreateShader) LOAD(DeleteBuffers) LOAD(DeleteProgram) LOAD(DeleteShader)
    LOAD(DeleteTextures) LOAD(Disable) LOAD(DisableVertexAttribArray) LOAD(DrawArrays)
    LOAD(Enable) LOAD(EnableVertexAttribArray) LOAD(GenBuffers) LOAD(GenTextures)
    LOAD(GetError) LOAD(GetIntegerv) LOAD(GetProgramiv) LOAD(GetShaderiv) LOAD(GetString)
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

/* Limited-range BT.709, matching the encoder's colour negotiation. Chroma
 * from NV12's RG plane, or (planar, 4:4:4) from separate Cb and Cr planes. */
static const char *FRAGMENT =
    "#version 120\n"
    "uniform sampler2D y_plane;\n"
    "uniform sampler2D uv_plane;\n"
    "uniform sampler2D v_plane;\n"
    "uniform int planar;\n"
    "varying vec2 coord;\n"
    "void main() {\n"
    "  float y = (texture2D(y_plane, coord).r - 16.0 / 255.0) * (255.0 / 219.0);\n"
    "  vec2 uv = planar == 1 ? vec2(texture2D(uv_plane, coord).r, texture2D(v_plane, coord).r)\n"
    "                        : texture2D(uv_plane, coord).rg;\n"
    "  vec2 c = (uv - 128.0 / 255.0) * (255.0 / 224.0);\n"
    "  vec3 rgb = vec3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);\n"
    "  gl_FragColor = vec4(clamp(rgb, 0.0, 1.0), 1.0);\n"
    "}\n";

static GLuint shader(TpGl *gl, GLenum type, const char *source) {
    GLuint id = gl->CreateShader(type);
    gl->ShaderSource(id, 1, &source, NULL);
    gl->CompileShader(id);
    GLint ok = 0;
    gl->GetShaderiv(id, GL_COMPILE_STATUS, &ok);
    if (!ok) { gl->DeleteShader(id); return 0; }
    return id;
}

void tp_nv12_gl_destroy(TpNv12Gl *g) {
    if (!g || !g->gl.DeleteTextures) return;
    if (g->textures[0]) g->gl.DeleteTextures(3, g->textures);
    if (g->vbo) g->gl.DeleteBuffers(1, &g->vbo);
    if (g->program) g->gl.DeleteProgram(g->program);
    g->textures[0] = g->textures[1] = g->textures[2] = g->vbo = g->program = 0;
}

int tp_nv12_gl_init(TpNv12Gl *g, TpGetProc get) {
    memset(g, 0, sizeof *g);
    if (!get || !load_gl(&g->gl, get)) return AVERROR(ENOSYS);
    TpGl *gl = &g->gl;
    GLint saved_program = 0, saved_buffer = 0, saved_active = 0, saved_texture = 0;
    gl->GetIntegerv(GL_CURRENT_PROGRAM, &saved_program);
    gl->GetIntegerv(GL_ARRAY_BUFFER_BINDING, &saved_buffer);
    gl->GetIntegerv(GL_ACTIVE_TEXTURE, &saved_active);
    gl->ActiveTexture(GL_TEXTURE0);
    gl->GetIntegerv(GL_TEXTURE_BINDING_2D, &saved_texture);
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
        g->v_plane = gl->GetUniformLocation(g->program, "v_plane");
        g->planar_location = gl->GetUniformLocation(g->program, "planar");
        static const GLfloat quad[] = {-1, -1, 1, -1, -1, 1, 1, 1};
        gl->GenBuffers(1, &g->vbo);
        gl->BindBuffer(GL_ARRAY_BUFFER, g->vbo);
        gl->BufferData(GL_ARRAY_BUFFER, sizeof quad, quad, GL_STATIC_DRAW);
        gl->GenTextures(3, g->textures);
        for (int i = 0; i < 3; i++) {
            gl->BindTexture(GL_TEXTURE_2D, g->textures[i]);
            gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            gl->TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        }
    }
    gl->BindTexture(GL_TEXTURE_2D, (GLuint)saved_texture);
    gl->ActiveTexture((GLenum)saved_active);
    gl->BindBuffer(GL_ARRAY_BUFFER, (GLuint)saved_buffer);
    gl->UseProgram((GLuint)saved_program);
    if (!g->program || gl->GetError() != GL_NO_ERROR) {
        tp_nv12_gl_destroy(g);
        return AVERROR_EXTERNAL;
    }
    return 0;
}

int tp_nv12_gl_draw(TpNv12Gl *g, TpNv12Upload upload, void *presenter, const AVFrame *frame,
    int x, int y, int width, int height, int framebuffer_height) {
    if (!g || !g->program || !frame || width <= 0 || height <= 0) return AVERROR(EINVAL);
    TpGl *gl = &g->gl;
    GLint program, buffer, active, viewport[4], bound[3];
    gl->GetIntegerv(GL_CURRENT_PROGRAM, &program);
    gl->GetIntegerv(GL_ARRAY_BUFFER_BINDING, &buffer);
    gl->GetIntegerv(GL_ACTIVE_TEXTURE, &active);
    gl->GetIntegerv(GL_VIEWPORT, viewport);
    for (int i = 0; i < 3; i++) {
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

    int result = upload(presenter, frame);
    if (result == 0) {
        gl->UseProgram(g->program);
        for (int i = 0; i < 3; i++) {
            gl->ActiveTexture(GL_TEXTURE0 + i);
            gl->BindTexture(GL_TEXTURE_2D, g->textures[i]);
        }
        gl->Uniform1i(g->y_plane, 0);
        gl->Uniform1i(g->uv_plane, 1);
        gl->Uniform1i(g->v_plane, 2);
        gl->Uniform1i(g->planar_location, g->planar);
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
    for (int i = 2; i >= 0; i--) {
        gl->ActiveTexture(GL_TEXTURE0 + i);
        gl->BindTexture(GL_TEXTURE_2D, (GLuint)bound[i]);
    }
    gl->ActiveTexture((GLenum)active);
    gl->UseProgram((GLuint)program);
    return result;
}

/* ---- Offscreen self-test support ---- */

typedef void *EGLDisplay;
typedef void *EGLContext;
typedef void *EGLConfig;
typedef void *EGLDeviceEXT;
typedef int EGLint;
typedef unsigned int EGLBoolean;
#define EGL_NONE 0x3038
#define EGL_DONT_CARE (-1)
#define EGL_SURFACE_TYPE 0x3033
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_BIT 0x0008
#define EGL_OPENGL_API 0x30A2
#define EGL_VENDOR 0x3053
#define EGL_PLATFORM_DEVICE_EXT 0x313F
#define EGL_DRM_RENDER_NODE_FILE_EXT 0x3377

static void *egl_library;
static void *(*egl_get_proc)(const char *);

void *tp_egl_proc(const char *name) {
    if (!egl_library) egl_library = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!egl_library) return NULL;
    if (!egl_get_proc) egl_get_proc = (void *(*)(const char *))dlsym(egl_library, "eglGetProcAddress");
    void *symbol = egl_get_proc ? egl_get_proc(name) : NULL;
    return symbol ? symbol : dlsym(egl_library, name);
}

int tp_egl_offscreen(TpEglMatch match, TpEglDraw draw, void *context_data, int width, int height,
    uint8_t *rgba) {
    EGLBoolean (*query)(EGLint, EGLDeviceEXT *, EGLint *) = tp_egl_proc("eglQueryDevicesEXT");
    const char *(*device_string)(EGLDeviceEXT, EGLint) = tp_egl_proc("eglQueryDeviceStringEXT");
    EGLDisplay (*platform)(EGLint, void *, const EGLint *) = tp_egl_proc("eglGetPlatformDisplayEXT");
    EGLBoolean (*initialize)(EGLDisplay, EGLint *, EGLint *) = tp_egl_proc("eglInitialize");
    const char *(*query_string)(EGLDisplay, EGLint) = tp_egl_proc("eglQueryString");
    EGLBoolean (*bind_api)(unsigned) = tp_egl_proc("eglBindAPI");
    EGLBoolean (*choose)(EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *) = tp_egl_proc("eglChooseConfig");
    EGLContext (*create)(EGLDisplay, EGLConfig, EGLContext, const EGLint *) = tp_egl_proc("eglCreateContext");
    EGLBoolean (*make_current)(EGLDisplay, void *, void *, EGLContext) = tp_egl_proc("eglMakeCurrent");
    EGLBoolean (*destroy)(EGLDisplay, EGLContext) = tp_egl_proc("eglDestroyContext");
    if (!query || !device_string || !platform || !initialize || !query_string || !bind_api
        || !choose || !create || !make_current || !destroy) return AVERROR(ENODEV);
    EGLDeviceEXT devices[16];
    EGLint count = 0;
    if (!query(16, devices, &count)) return AVERROR(ENODEV);
    for (int i = 0; i < count; i++) {
        const char *node = device_string(devices[i], EGL_DRM_RENDER_NODE_FILE_EXT);
        EGLDisplay display = platform(EGL_PLATFORM_DEVICE_EXT, devices[i], NULL);
        EGLint major, minor;
        if (!display || !initialize(display, &major, &minor)) continue;
        const char *vendor = query_string(display, EGL_VENDOR);
        if (!vendor || !match(context_data, vendor, node)) continue;
        bind_api(EGL_OPENGL_API);
        /* Surfaceless: rendering goes to an FBO. */
        EGLint attributes[] = {EGL_SURFACE_TYPE, EGL_DONT_CARE, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
        EGLConfig config;
        EGLint configs = 0;
        if (!choose(display, attributes, &config, 1, &configs) || configs < 1) continue;
        EGLContext context = create(display, config, NULL, NULL);
        if (!context) continue;
        if (!make_current(display, NULL, NULL, context)) { destroy(display, context); continue; }

        void (*gen_framebuffers)(GLsizei, GLuint *) = tp_egl_proc("glGenFramebuffers");
        void (*delete_framebuffers)(GLsizei, const GLuint *) = tp_egl_proc("glDeleteFramebuffers");
        void (*bind_framebuffer)(GLenum, GLuint) = tp_egl_proc("glBindFramebuffer");
        void (*framebuffer_texture)(GLenum, GLenum, GLenum, GLuint, GLint) = tp_egl_proc("glFramebufferTexture2D");
        GLenum (*status)(GLenum) = tp_egl_proc("glCheckFramebufferStatus");
        void (*read_pixels)(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, void *) = tp_egl_proc("glReadPixels");
        void (*pixel_store)(GLenum, GLint) = tp_egl_proc("glPixelStorei");
        void (*gen_textures)(GLsizei, GLuint *) = tp_egl_proc("glGenTextures");
        void (*delete_textures)(GLsizei, const GLuint *) = tp_egl_proc("glDeleteTextures");
        void (*bind_texture)(GLenum, GLuint) = tp_egl_proc("glBindTexture");
        void (*tex_image)(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *) = tp_egl_proc("glTexImage2D");
        int result = AVERROR(ENODEV);
        if (gen_framebuffers && delete_framebuffers && bind_framebuffer && framebuffer_texture
            && status && read_pixels && pixel_store && gen_textures && delete_textures
            && bind_texture && tex_image) {
            GLuint target, framebuffer;
            gen_textures(1, &target);
            bind_texture(GL_TEXTURE_2D, target);
            tex_image(GL_TEXTURE_2D, 0, GL_RGBA8, width, height, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
            gen_framebuffers(1, &framebuffer);
            bind_framebuffer(GL_FRAMEBUFFER, framebuffer);
            framebuffer_texture(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target, 0);
            result = AVERROR_EXTERNAL;
            if (status(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE) {
                result = draw(context_data, tp_egl_proc);
                if (result == 0) {
                    pixel_store(GL_PACK_ALIGNMENT, 1);
                    read_pixels(0, 0, width, height, GL_RGBA, GL_UNSIGNED_BYTE, rgba);
                }
            }
            bind_framebuffer(GL_FRAMEBUFFER, 0);
            delete_framebuffers(1, &framebuffer);
            delete_textures(1, &target);
        }
        make_current(display, NULL, NULL, NULL);
        destroy(display, context);
        return result;
    }
    return AVERROR(ENODEV);
}

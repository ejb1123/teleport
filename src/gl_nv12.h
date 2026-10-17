/* Shared NV12 OpenGL presentation used by the zero-copy presenters
 * (cuda_gl.c: CUDA copies into GL textures; vaapi_egl.c: VA-API surfaces
 * imported as EGLImages). Draws limited-range BT.709 NV12 from two textures
 * (R8 luma, RG8 chroma) and restores all GL state it touches.
 */
#ifndef TP_GL_NV12_H
#define TP_GL_NV12_H

#include <libavutil/frame.h>
#include <stddef.h>
#include <stdint.h>

typedef unsigned int GLenum;
typedef unsigned int GLuint;
typedef int GLint;
typedef int GLsizei;
typedef unsigned char GLboolean;
typedef char GLchar;
typedef float GLfloat;
typedef ptrdiff_t GLsizeiptr;

#define GL_TEXTURE_2D 0x0DE1
#define GL_TEXTURE0 0x84C0
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
#define GL_UNSIGNED_BYTE 0x1401
#define GL_UNSIGNED_SHORT 0x1403
#define GL_R16_EXT 0x822A
#define GL_RG16_EXT 0x822C
#define GL_NO_ERROR 0
#define GL_VENDOR 0x1F00

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
    const unsigned char *(*GetString)(GLenum);
    GLint (*GetUniformLocation)(GLuint, const GLchar *);
    void (*GetVertexAttribiv)(GLuint, GLenum, GLint *);
    void (*GetVertexAttribPointerv)(GLuint, GLenum, void **);
    GLboolean (*IsEnabled)(GLenum);
    void (*LinkProgram)(GLuint);
    void (*ShaderSource)(GLuint, GLsizei, const GLchar *const *, const GLint *);
    void (*TexImage2D)(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *);
    void (*TexParameteri)(GLenum, GLenum, GLint);
    void (*Uniform1f)(GLint, GLfloat);
    void (*Uniform1i)(GLint, GLint);
    void (*UseProgram)(GLuint);
    void (*VertexAttribPointer)(GLuint, GLint, GLenum, GLboolean, GLsizei, const void *);
    void (*Viewport)(GLint, GLint, GLsizei, GLsizei);
} TpGl;

typedef struct {
    TpGl gl;
    /* NV12: textures 0 (R8 luma) and 1 (RG8 chroma). With `planar` set by
     * the upload (4:4:4), three full-size R8 textures: Y, Cb, Cr. */
    GLuint program, vbo, textures[3];
    GLint y_plane, uv_plane, v_plane, planar_location, dither_location;
    /* Set by the upload: 4:4:4 planes, 10-bit (16-bit) planes. */
    int planar, ten_bit;
} TpNv12Gl;

/* Fills `textures` with the frame's planes. Called by tp_nv12_gl_draw after
 * GL state has been saved, so it may bind textures freely. */
typedef int (*TpNv12Upload)(void *presenter, const AVFrame *frame);

/* Resolve GL entry points, compile the shader and create three linear,
 * edge-clamped textures in the current context. 0 on success. */
int tp_nv12_gl_init(TpNv12Gl *nv12, TpGetProc get);
void tp_nv12_gl_destroy(TpNv12Gl *nv12);

/* Run `upload`, then draw `nv12->textures` at (x, y, width, height) in
 * framebuffer pixels with a top-left origin. Restores all GL state. */
int tp_nv12_gl_draw(TpNv12Gl *nv12, TpNv12Upload upload, void *presenter, const AVFrame *frame,
    int x, int y, int width, int height, int framebuffer_height);

/* ---- Offscreen EGL self-test support (no window, nothing linked). ---- */

/* Returns 1 when the EGL display (`vendor`, DRM render node `node` or NULL)
 * is the device the test needs. */
typedef int (*TpEglMatch)(void *context, const char *vendor, const char *node);
/* Draws into the current framebuffer (an RGBA8 texture of width x height). */
typedef int (*TpEglDraw)(void *context, TpGetProc get);

void *tp_egl_proc(const char *name);
/* Make a GL context current on the first matching EGL device, attach an
 * RGBA8 framebuffer, run `draw` and read it back into `rgba` (bottom-up
 * rows). AVERROR(ENODEV) when no device matches. */
int tp_egl_offscreen(TpEglMatch match, TpEglDraw draw, void *context, int width, int height,
    uint8_t *rgba);

#endif

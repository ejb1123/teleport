#define GL_GLEXT_PROTOTYPES
#include <assert.h>
#include <stdio.h>
#include <stdint.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>
int main(void) {
 PFNEGLQUERYDEVICESEXTPROC devices = (void *)eglGetProcAddress("eglQueryDevicesEXT");
 PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
 EGLDeviceEXT device[8]; EGLint device_count=0;
 assert(devices && platform && devices(8,device,&device_count) && device_count);
 EGLDisplay d = platform(EGL_PLATFORM_DEVICE_EXT, device[0], NULL);
 assert(eglInitialize(d, NULL, NULL));
 assert(eglBindAPI(EGL_OPENGL_API));
 const EGLint attribs[] = {EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_BIT,EGL_NONE};
 EGLConfig config; EGLint count;
 assert(eglChooseConfig(d,attribs,&config,1,&count) && count);
 const EGLint pbuffer[] = {EGL_WIDTH,16,EGL_HEIGHT,16,EGL_NONE};
 EGLSurface surface = eglCreatePbufferSurface(d,config,pbuffer);
 EGLContext context = eglCreateContext(d,config,EGL_NO_CONTEXT,NULL);
 assert(eglMakeCurrent(d,surface,surface,context));
 printf("GL renderer: %s\n",glGetString(GL_RENDERER));
 GLuint tex, fbo;
 glGenTextures(1,&tex); glBindTexture(GL_TEXTURE_2D,tex);
 glTexImage2D(GL_TEXTURE_2D,0,GL_RGB10_A2,16,16,0,GL_RGBA,GL_UNSIGNED_INT_2_10_10_10_REV,NULL);
 glGenFramebuffers(1,&fbo); glBindFramebuffer(GL_FRAMEBUFFER,fbo);
 glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,tex,0);
 assert(glCheckFramebufferStatus(GL_FRAMEBUFFER)==GL_FRAMEBUFFER_COMPLETE);
 GLint red; glGetFramebufferAttachmentParameteriv(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_FRAMEBUFFER_ATTACHMENT_RED_SIZE,&red);
 assert(red==10);
 glDisable(GL_DITHER);
 for (unsigned n=0;n<1024;++n) {
   glClearColor(n/1023.0f,(1023-n)/1023.0f,0.0f,1.0f); glClear(GL_COLOR_BUFFER_BIT);
   uint32_t word=0;
   glReadPixels(0,0,1,1,GL_RGBA,GL_UNSIGNED_INT_2_10_10_10_REV,&word);
   assert(glGetError()==GL_NO_ERROR);
   assert((word&1023)==n); assert(((word>>10)&1023)==1023-n); assert(((word>>20)&1023)==0);
 }
 glDeleteFramebuffers(1,&fbo); glDeleteTextures(1,&tex);
 eglMakeCurrent(d,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
 eglDestroyContext(d,context); eglDestroySurface(d,surface); eglTerminate(d);
 puts("All 1024 levels survived GL_RGB10_A2 framebuffer + packed RGBA readback");
}

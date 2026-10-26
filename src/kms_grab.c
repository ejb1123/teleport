/* KMS scanout capture (`--source kms`): the framebuffer a CRTC is showing,
 * exported as DMA-BUFs, the way FFmpeg's kmsgrab device does it. For hosts
 * without a desktop portal or X server, such as a kiosk compositor that owns
 * the display directly. Linux only.
 *
 * Reading another client's framebuffer handles needs DRM master or
 * CAP_SYS_ADMIN; without either the kernel reports the framebuffer but zero
 * handles, which tp_kms_grab returns as -EPERM.
 */
#define _POSIX_C_SOURCE 200809L /* open flags under -std=c11 */
#include "kms_grab.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

int tp_kms_open(const char *path) {
    int fd = open(path, O_RDWR | O_CLOEXEC);
    return fd < 0 ? -errno : fd;
}

/* The connector driving `crtc_id`, named like the kernel does (HDMI-A-1). */
static void connector_name(int fd, drmModeRes *resources, uint32_t crtc_id, char *name,
                           size_t size) {
    snprintf(name, size, "CRTC %u", crtc_id);
    for (int i = 0; i < resources->count_connectors; i++) {
        drmModeConnector *connector = drmModeGetConnectorCurrent(fd, resources->connectors[i]);
        if (!connector)
            continue;
        uint32_t current = 0;
        if (connector->connection == DRM_MODE_CONNECTED && connector->encoder_id) {
            drmModeEncoder *encoder = drmModeGetEncoder(fd, connector->encoder_id);
            if (encoder) {
                current = encoder->crtc_id;
                drmModeFreeEncoder(encoder);
            }
        }
        if (current == crtc_id) {
            const char *type = drmModeGetConnectorTypeName(connector->connector_type);
            snprintf(name, size, "%s-%u", type ? type : "Unknown", connector->connector_type_id);
            drmModeFreeConnector(connector);
            return;
        }
        drmModeFreeConnector(connector);
    }
}

int tp_kms_outputs(int fd, TpKmsOutput *outputs, int capacity) {
    drmModeRes *resources = drmModeGetResources(fd);
    if (!resources)
        return errno ? -errno : -ENODEV;
    int count = 0;
    for (int i = 0; i < resources->count_crtcs && count < capacity; i++) {
        drmModeCrtc *crtc = drmModeGetCrtc(fd, resources->crtcs[i]);
        if (!crtc)
            continue;
        if (crtc->mode_valid && crtc->buffer_id) {
            TpKmsOutput *output = &outputs[count++];
            memset(output, 0, sizeof *output);
            output->crtc_id = crtc->crtc_id;
            output->width = crtc->mode.hdisplay;
            output->height = crtc->mode.vdisplay;
            /* Exact rate from the pixel clock; vrefresh is rounded. */
            uint64_t pixels = (uint64_t)crtc->mode.htotal * crtc->mode.vtotal;
            output->refresh_mhz =
                pixels ? (uint32_t)((uint64_t)crtc->mode.clock * 1000000 / pixels)
                       : crtc->mode.vrefresh * 1000;
            connector_name(fd, resources, crtc->crtc_id, output->name, sizeof output->name);
        }
        drmModeFreeCrtc(crtc);
    }
    drmModeFreeResources(resources);
    return count;
}

static void close_handle(int fd, uint32_t handle) {
    struct drm_gem_close request = {.handle = handle};
    drmIoctl(fd, DRM_IOCTL_GEM_CLOSE, &request);
}

int tp_kms_grab(int fd, uint32_t crtc_id, TpKmsFrame *frame) {
    memset(frame, 0, sizeof *frame);
    for (int i = 0; i < 4; i++)
        frame->fds[i] = -1;
    drmModeCrtc *crtc = drmModeGetCrtc(fd, crtc_id);
    if (!crtc)
        return errno ? -errno : -ENOENT;
    /* The primary plane's current framebuffer: it changes on every page
     * flip, so it is read again for each frame. */
    uint32_t framebuffer = crtc->buffer_id;
    drmModeFreeCrtc(crtc);
    if (!framebuffer)
        return -ENOENT;
    drmModeFB2 *fb = drmModeGetFB2(fd, framebuffer);
    if (!fb)
        return errno ? -errno : -ENOENT;
    int result = 0;
    if (!fb->handles[0]) {
        result = -EPERM;
        goto out;
    }
    frame->fourcc = fb->pixel_format;
    frame->modifier = (fb->flags & DRM_MODE_FB_MODIFIERS) ? fb->modifier : DRM_FORMAT_MOD_INVALID;
    frame->width = fb->width;
    frame->height = fb->height;
    for (int i = 0; i < 4 && fb->handles[i]; i++) {
        /* Planes of one buffer object share a handle: export it once. */
        int shared = i;
        for (int j = i - 1; j >= 0; j--)
            if (fb->handles[j] == fb->handles[i])
                shared = frame->source_plane[j];
        frame->source_plane[i] = shared;
        if (shared == i && drmPrimeHandleToFD(fd, fb->handles[i], DRM_CLOEXEC, &frame->fds[i]) < 0) {
            frame->fds[i] = -1;
            result = errno ? -errno : -EIO;
            break;
        }
        frame->offsets[i] = fb->offsets[i];
        frame->pitches[i] = fb->pitches[i];
        frame->planes = i + 1;
    }
out:
    for (int i = 0; i < 4; i++) {
        int seen = 0;
        for (int j = 0; j < i; j++)
            seen |= fb->handles[j] == fb->handles[i];
        if (fb->handles[i] && !seen)
            close_handle(fd, fb->handles[i]);
    }
    drmModeFreeFB2(fb);
    if (result < 0)
        tp_kms_frame_close(frame);
    return result;
}

void tp_kms_frame_close(TpKmsFrame *frame) {
    for (int i = 0; i < 4; i++) {
        if (frame->fds[i] >= 0)
            close(frame->fds[i]);
        frame->fds[i] = -1;
    }
    frame->planes = 0;
}

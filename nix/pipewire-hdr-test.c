/* Standalone negotiation regression test for the application-local plugin patch. */
#include <assert.h>
#include <stdint.h>
#include <gst/video/video.h>
#include <spa/param/video/format-utils.h>
#include "gstpipewireformat.h"

static void check_caps(const char *description, enum spa_video_format expected)
{
    GstCaps *caps = gst_caps_from_string(description);
    GPtrArray *formats = gst_caps_to_format_all(caps);
    assert(formats && formats->len == 1);
    struct spa_video_info_raw video = {0};
    struct spa_pod *pod = g_ptr_array_index(formats, 0);
    /* A single-element format choice is already fixed by the converter. */
    assert(spa_format_video_raw_parse(pod, &video) >= 0);
    assert(video.format == expected);
    GstCaps *roundtrip = gst_caps_from_format(pod);
    assert(roundtrip && gst_caps_can_intersect(caps, roundtrip));
    if (expected == SPA_VIDEO_FORMAT_ABGR_210LE) {
        assert(video.color_range == SPA_VIDEO_COLOR_RANGE_0_255);
        assert(video.color_matrix == SPA_VIDEO_COLOR_MATRIX_RGB);
        assert(video.transfer_function == SPA_VIDEO_TRANSFER_SMPTE2084);
        assert(video.color_primaries == SPA_VIDEO_COLOR_PRIMARIES_BT2020);
        GstVideoColorimetry color;
        assert(gst_video_colorimetry_from_string(&color,
            gst_structure_get_string(gst_caps_get_structure(roundtrip, 0), "colorimetry")));
        assert(color.range == GST_VIDEO_COLOR_RANGE_0_255);
        assert(color.matrix == GST_VIDEO_COLOR_MATRIX_RGB);
        assert(color.transfer == GST_VIDEO_TRANSFER_SMPTE2084);
        assert(color.primaries == GST_VIDEO_COLOR_PRIMARIES_BT2020);
    }
    gst_caps_unref(roundtrip);
    g_ptr_array_unref(formats);
    gst_caps_unref(caps);
}

int main(int argc, char **argv)
{
    gst_init(&argc, &argv);
    check_caps("video/x-raw,format=RGB10A2_LE,colorimetry=1:1:14:7,width=1920,height=1080,framerate=60/1", SPA_VIDEO_FORMAT_ABGR_210LE);
    check_caps("video/x-raw,format=BGRA,width=1920,height=1080,framerate=60/1", SPA_VIDEO_FORMAT_BGRA);
    /* GStreamer's unpack function must read exactly the compositor's packed
     * GL_RGBA / UNSIGNED_INT_2_10_10_10_REV byte layout. */
    const GstVideoFormatInfo *info = gst_video_format_get_info(GST_VIDEO_FORMAT_RGB10A2_LE);
    assert(info && info->unpack_func && info->unpack_format == GST_VIDEO_FORMAT_ARGB64);
    for (int channel = 0; channel < 3; ++channel) {
        const uint32_t word = GUINT32_TO_LE((3u << 30) | (1023u << (channel * 10)));
        const gpointer planes[4] = { (gpointer)&word, NULL, NULL, NULL };
        const gint strides[4] = {4, 0, 0, 0};
        guint16 unpacked[4] = {0};
        info->unpack_func(info, 0, unpacked, planes, strides, 0, 0, 1);
        /* Alpha is discarded by the opaque desktop-to-YUV conversion. */
        for (int component = 0; component < 3; ++component)
            assert(unpacked[component + 1] == (component == channel ? 65535 : 0));
    }
    g_print("HDR RGB10A2_LE/PQ/full-range negotiation roundtrip and SDR regression passed\n");
    return 0;
}

#include "screencast-client.h"
#include <assert.h>
#include <gst/app/gstappsink.h>
#include <gst/video/video.h>
#include <math.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wayland-client.h>
static struct zkde_screencast_unstable_v1 *manager;
static struct wl_output *output;
static uint32_t node;
static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
  if (!strcmp(interface, "zkde_screencast_unstable_v1"))
    manager = wl_registry_bind(registry, name,
                               &zkde_screencast_unstable_v1_interface, 3);
  if (!strcmp(interface, "wl_output") && !output)
    output = wl_registry_bind(registry, name, &wl_output_interface, 1);
}
static void removed(void *data, struct wl_registry *registry, uint32_t name) {}
static void closed(void *data,
                   struct zkde_screencast_stream_unstable_v1 *stream) {
  fprintf(stderr, "stream closed\n");
}
static void created(void *data,
                    struct zkde_screencast_stream_unstable_v1 *stream,
                    uint32_t id) {
  node = id;
}
static void failed(void *data,
                   struct zkde_screencast_stream_unstable_v1 *stream,
                   const char *error) {
  fprintf(stderr, "screencast: %s\n", error);
  exit(2);
}
int main(int argc, char **argv) {
  gst_init(&argc, &argv);
  const int hdr = argc > 1 && !strcmp(argv[1], "hdr");
  struct wl_display *display = wl_display_connect(NULL);
  assert(display);
  struct wl_registry *registry = wl_display_get_registry(display);
  const struct wl_registry_listener registry_listener = {global, removed};
  wl_registry_add_listener(registry, &registry_listener, NULL);
  wl_display_roundtrip(display);
  if (!manager || !output) {
    fprintf(stderr,
            "missing restricted screencast or output global (manager=%d "
            "output=%d)\n",
            !!manager, !!output);
    return 3;
  }
  struct zkde_screencast_stream_unstable_v1 *stream =
      zkde_screencast_unstable_v1_stream_output(manager, output, 1);
  const struct zkde_screencast_stream_unstable_v1_listener listener = {
      .closed = closed, .created = created, .failed = failed};
  zkde_screencast_stream_unstable_v1_add_listener(stream, &listener, NULL);
  wl_display_roundtrip(display);
  for (int i = 0; !node && i < 50; ++i) {
    struct pollfd fd = {wl_display_get_fd(display), POLLIN, 0};
    wl_display_flush(display);
    if (poll(&fd, 1, 100) > 0)
      assert(wl_display_dispatch(display) >= 0);
  }
  assert(node);
  char *description = g_strdup_printf(
      "pipewiresrc path=%u do-timestamp=true ! video/x-raw,format=%s%s ! "
      "appsink name=sink sync=false max-buffers=1 drop=true",
      node, hdr ? "RGB10A2_LE" : "BGRA", hdr ? ",colorimetry=1:1:14:7" : "");
  GError *error = NULL;
  GstElement *pipeline = gst_parse_launch(description, &error);
  assert(pipeline && !error);
  g_free(description);
  GstElement *sink = gst_bin_get_by_name(GST_BIN(pipeline), "sink");
  gst_element_set_state(pipeline, GST_STATE_PLAYING);
  GstSample *sample =
      gst_app_sink_try_pull_sample(GST_APP_SINK(sink), 10 * GST_SECOND);
  if (!sample) {
    GstBus *bus = gst_element_get_bus(pipeline);
    GstMessage *msg = gst_bus_pop_filtered(bus, GST_MESSAGE_ERROR);
    if (msg) {
      gchar *debug = NULL;
      gst_message_parse_error(msg, &error, &debug);
      fprintf(stderr, "pipeline: %s (%s)\n", error->message,
              debug ? debug : "");
    }
    fprintf(stderr, "no capture sample\n");
    return 4;
  }
  GstCaps *caps = gst_sample_get_caps(sample);
  gchar *caps_text = gst_caps_to_string(caps);
  printf("negotiated: %s\n", caps_text);
  g_free(caps_text);
  GstVideoInfo info;
  assert(gst_video_info_from_caps(&info, caps));
  assert(GST_VIDEO_INFO_FORMAT(&info) ==
         (hdr ? GST_VIDEO_FORMAT_RGB10A2_LE : GST_VIDEO_FORMAT_BGRA));
  if (hdr) {
    assert(info.colorimetry.transfer == GST_VIDEO_TRANSFER_SMPTE2084);
    assert(info.colorimetry.primaries == GST_VIDEO_COLOR_PRIMARIES_BT2020);
    assert(info.colorimetry.range == GST_VIDEO_COLOR_RANGE_0_255);
    assert(info.colorimetry.matrix == GST_VIDEO_COLOR_MATRIX_RGB);
  }
  const double normalized = pow(203.0 / 10000.0, 2610.0 / 16384.0);
  const unsigned expected =
      hdr ? lround(1023.0 *
                   pow((3424.0 / 4096.0 + (2413.0 / 128.0) * normalized) /
                           (1.0 + (2392.0 / 128.0) * normalized),
                       2523.0 / 32.0))
          : 255;
  unsigned rgb[3] = {0};
  int matched = 0;
  for (int attempt = 0; attempt < 20; ++attempt) {
    if (!sample)
      sample = gst_app_sink_try_pull_sample(GST_APP_SINK(sink), GST_SECOND / 2);
    if (!sample)
      continue;
    GstVideoFrame frame;
    assert(gst_video_frame_map(&frame, &info, gst_sample_get_buffer(sample),
                               GST_MAP_READ));
    const uint8_t *pixel =
        (const uint8_t *)GST_VIDEO_FRAME_PLANE_DATA(&frame, 0) +
        (info.height / 2) * GST_VIDEO_FRAME_PLANE_STRIDE(&frame, 0) +
        (info.width / 2) * 4;
    const uint32_t word = GST_READ_UINT32_LE(pixel);
    rgb[0] = hdr ? (word & 1023) : pixel[2];
    rgb[1] = hdr ? ((word >> 10) & 1023) : pixel[1];
    rgb[2] = hdr ? ((word >> 20) & 1023) : pixel[0];
    matched = abs((int)rgb[0] - (int)expected) <= 3 &&
              abs((int)rgb[1] - (int)expected) <= 3 &&
              abs((int)rgb[2] - (int)expected) <= 3;
    gst_video_frame_unmap(&frame);
    gst_sample_unref(sample);
    sample = NULL;
    if (matched)
      break;
  }
  printf("%s center white: RGB %u/%u/%u, expected %u (+/-3), %s\n",
         hdr ? "HDR" : "SDR", rgb[0], rgb[1], rgb[2], expected,
         matched ? "PASS" : "FAIL");
  gst_element_set_state(pipeline, GST_STATE_NULL);
  gst_object_unref(sink);
  gst_object_unref(pipeline);
  zkde_screencast_stream_unstable_v1_close(stream);
  wl_display_roundtrip(display);
  wl_display_disconnect(display);
  return matched ? 0 : 5;
}

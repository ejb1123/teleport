/* Exercises the exact patched helper, without Apple frameworks or a display. */
#define GST_USE_UNSTABLE_API
#include <gst/app/gstappsink.h>
#include "gst-vtdec-hevc-reorder.h"

static void
check_stream (gboolean reordered, gboolean main10)
{
  gchar *description = g_strdup_printf (
      "videotestsrc num-buffers=12 ! video/x-raw,format=%s,width=320,height=180,framerate=30/1 "
      "! x265enc speed-preset=ultrafast option-string=pools=none:frame-threads=1:log-level=error:bframes=%d "
      "! h265parse ! video/x-h265,stream-format=hvc1,alignment=au "
      "! appsink name=out sync=false max-buffers=1 drop=true",
      main10 ? "I420_10LE" : "I420", reordered ? 3 : 0);
  GError *error = NULL;
  GstElement *pipeline = gst_parse_launch (description, &error);
  g_free (description);
  g_assert_no_error (error);
  g_assert_nonnull (pipeline);
  GstElement *sink = gst_bin_get_by_name (GST_BIN (pipeline), "out");
  g_assert_true (gst_element_set_state (pipeline, GST_STATE_PLAYING) != GST_STATE_CHANGE_FAILURE);
  GstSample *sample = gst_app_sink_try_pull_sample (GST_APP_SINK (sink), 10 * GST_SECOND);
  g_assert_nonnull (sample);
  const GValue *value = gst_structure_get_value (
      gst_caps_get_structure (gst_sample_get_caps (sample), 0), "codec_data");
  g_assert_nonnull (value);
  gint depth = -1;
  g_assert_true (gst_vtdec_hevc_reorder_depth (gst_value_get_buffer (value), &depth));
  if (reordered)
    g_assert_cmpint (depth, >, 0);
  else
    g_assert_cmpint (depth, ==, 0);
  g_print ("HEVC %s %s reorder depth: %d\n", main10 ? "Main10" : "Main",
      reordered ? "B-frames" : "low-delay", depth);
  gst_sample_unref (sample);
  gst_element_set_state (pipeline, GST_STATE_NULL);
  gst_object_unref (sink);
  gst_object_unref (pipeline);
}

int main (int argc, char **argv)
{
  gst_init (&argc, &argv);
  g_assert_cmpint (gst_vtdec_hevc_queue_threshold (0), ==, 0);
  g_assert_cmpint (gst_vtdec_hevc_queue_threshold (2), ==, 3);
  /* Model vtdec's sorted output queue and its >= threshold condition. A depth
   * of two needs three frames before emission, or [2,1,0] emits 1 before 0. */
  gint input[] = {2, 1, 0, 5, 4, 3}, queue[6], queued = 0, expected = 0;
  for (guint i = 0; i < G_N_ELEMENTS (input); i++) {
    gint at = queued++;
    while (at > 0 && queue[at - 1] > input[i]) {
      queue[at] = queue[at - 1];
      at--;
    }
    queue[at] = input[i];
    if (queued >= gst_vtdec_hevc_queue_threshold (2)) {
      g_assert_cmpint (queue[0], ==, expected++);
      memmove (queue, queue + 1, (gsize) --queued * sizeof (*queue));
    }
  }
  for (gint i = 0; i < queued; i++)
    g_assert_cmpint (queue[i], ==, expected++);
  g_assert_cmpint (expected, ==, 6);
  GstH265SPS sps = {0};
  gint depth = -1;
  g_assert_true (gst_vtdec_hevc_sps_reorder_depth (&sps, &depth));
  g_assert_cmpint (depth, ==, 0);
  sps.max_sub_layers_minus1 = 2;
  sps.max_dec_pic_buffering_minus1[1] = 4;
  sps.max_num_reorder_pics[1] = 3;
  g_assert_true (gst_vtdec_hevc_sps_reorder_depth (&sps, &depth));
  g_assert_cmpint (depth, ==, 3);
  sps.max_num_reorder_pics[1] = 5;
  g_assert_false (gst_vtdec_hevc_sps_reorder_depth (&sps, &depth));
  sps.max_sub_layers_minus1 = GST_H265_MAX_SUB_LAYERS;
  g_assert_false (gst_vtdec_hevc_sps_reorder_depth (&sps, &depth));
  g_assert_false (gst_vtdec_hevc_reorder_depth (NULL, &depth));
  GstBuffer *empty = gst_buffer_new_allocate (NULL, 23, NULL);
  gst_buffer_memset (empty, 0, 0, 23);
  g_assert_false (gst_vtdec_hevc_reorder_depth (empty, &depth));
  gst_buffer_unref (empty);
  for (gint main10 = 0; main10 < 2; main10++) {
    check_stream (FALSE, main10);
    check_stream (TRUE, main10);
  }
  return 0;
}

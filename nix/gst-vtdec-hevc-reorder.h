/* Application-local GStreamer 1.26 applemedia fix. VideoToolbox manages its
 * reference-picture DPB internally; vtdec's output queue needs only the SPS
 * display-reordering bound, not the level's maximum reference-picture count.
 * Kept independent of Apple frameworks so the exact logic is tested on Linux.
 */
#ifndef TELEPORT_GST_VTDEC_HEVC_REORDER_H
#define TELEPORT_GST_VTDEC_HEVC_REORDER_H
#include <gst/codecparsers/gsth265parser.h>

static gint
gst_vtdec_hevc_queue_threshold (gint reorder_depth)
{
  /* vtdec emits while queue length >= dbp_size. Retain reorder_depth frames
   * after each output, but preserve its established zero-delay fast path. */
  return reorder_depth == 0 ? 0 : reorder_depth + 1;
}

static gboolean
gst_vtdec_hevc_sps_reorder_depth (const GstH265SPS * sps, gint * depth)
{
  guint layer;
  gint maximum = 0;

  if (sps->max_sub_layers_minus1 >= GST_H265_MAX_SUB_LAYERS)
    return FALSE;
  for (layer = 0; layer <= sps->max_sub_layers_minus1; layer++) {
    /* Parsed/inferred values cover all temporal sublayers. Reject rather than
     * truncate an invalid bound: the caller retains its conservative fallback. */
    if (sps->max_dec_pic_buffering_minus1[layer] > 15 ||
        sps->max_num_reorder_pics[layer] > sps->max_dec_pic_buffering_minus1[layer])
      return FALSE;
    maximum = MAX (maximum, sps->max_num_reorder_pics[layer]);
  }
  *depth = maximum;
  return TRUE;
}

static gboolean
gst_vtdec_hevc_reorder_depth (GstBuffer * codec_data, gint * depth)
{
  GstH265Parser *parser;
  GstH265DecoderConfigRecord *config = NULL;
  GstMapInfo map;
  gboolean found = FALSE, success = FALSE;
  gint maximum = 0;
  guint i, j;

  if (!codec_data || !gst_buffer_map (codec_data, &map, GST_MAP_READ))
    return FALSE;
  parser = gst_h265_parser_new ();
  if (gst_h265_parser_parse_decoder_config_record (parser, map.data, map.size,
          &config) != GST_H265_PARSER_OK || !config || !config->nalu_array)
    goto done;

  /* Parse VPS first regardless of array order, so SPS links can be resolved. */
  for (i = 0; i < config->nalu_array->len; i++) {
    GstH265DecoderConfigRecordNalUnitArray *array =
        &g_array_index (config->nalu_array, GstH265DecoderConfigRecordNalUnitArray, i);
    if (array->nal_unit_type != GST_H265_NAL_VPS)
      continue;
    for (j = 0; j < array->nalu->len; j++) {
      GstH265NalUnit *nalu = &g_array_index (array->nalu, GstH265NalUnit, j);
      if (nalu->layer_id != 0 || gst_h265_parser_parse_nal (parser, nalu) != GST_H265_PARSER_OK)
        goto done;
    }
  }
  for (i = 0; i < config->nalu_array->len; i++) {
    GstH265DecoderConfigRecordNalUnitArray *array =
        &g_array_index (config->nalu_array, GstH265DecoderConfigRecordNalUnitArray, i);
    if (array->nal_unit_type != GST_H265_NAL_SPS)
      continue;
    for (j = 0; j < array->nalu->len; j++) {
      GstH265NalUnit *nalu = &g_array_index (array->nalu, GstH265NalUnit, j);
      GstH265SPS sps;
      gint current;
      if (nalu->layer_id != 0 ||
          gst_h265_parser_parse_sps (parser, nalu, &sps, FALSE) != GST_H265_PARSER_OK ||
          !gst_vtdec_hevc_sps_reorder_depth (&sps, &current))
        goto done;
      maximum = MAX (maximum, current);
      found = TRUE;
    }
  }
  if (found) {
    *depth = maximum;
    success = TRUE;
  }
done:
  if (config)
    gst_h265_decoder_config_record_free (config);
  gst_h265_parser_free (parser);
  gst_buffer_unmap (codec_data, &map);
  return success;
}
#endif

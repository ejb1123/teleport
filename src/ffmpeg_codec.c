/* Opaque codec ownership boundary. No FFmpeg ABI structs cross into Rust.
 * Each instance is serialized by its Rust owner. Output pointers live until
 * the next call on that instance and are copied before releasing its lock. */
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/imgutils.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <libswscale/swscale.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    AVCodecContext *ctx;
    AVPacket *packet;
    AVFrame *frame, *download, *converted;
    struct SwsContext *sws;
    enum AVPixelFormat hw_format, input_format;
    int encoder, hdr, output_format, width, height, fps, kbps;
    int live_bitrate; /* encoder applies bitrate changes in place */
} TpCodec;

typedef struct {
    const uint8_t *data;
    int size, width, height, key;
    int64_t pts;
    /* Output 3: a reference to the decoded CUDA or VA-API frame (no download). The
     * caller owns it and releases it with tp_gpu_frame_free. */
    AVFrame *gpu;
} TpOutput;

void tp_gpu_frame_free(AVFrame *frame) { av_frame_free(&frame); }

void tp_codec_free(TpCodec *codec) {
    if (!codec) return;
    avcodec_free_context(&codec->ctx);
    av_packet_free(&codec->packet);
    av_frame_free(&codec->frame);
    av_frame_free(&codec->download);
    av_frame_free(&codec->converted);
    sws_freeContext(codec->sws);
    free(codec);
}

static enum AVPixelFormat choose_hw(AVCodecContext *ctx, const enum AVPixelFormat *formats) {
    TpCodec *codec = ctx->opaque;
    for (; *formats != AV_PIX_FMT_NONE; ++formats)
        if (*formats == codec->hw_format) return *formats;
    return AV_PIX_FMT_NONE; /* Never claim hardware while silently decoding on CPU. */
}

static int dimensions(int width, int height) {
    return width >= 2 && height >= 2 && width <= 7680 && height <= 8192
        && !(width & 1) && !(height & 1) && (int64_t)width * height <= 33554432;
}

static int color_frame(AVFrame *frame, int hdr) {
    frame->color_range = AVCOL_RANGE_MPEG;
    frame->color_primaries = hdr ? AVCOL_PRI_BT2020 : AVCOL_PRI_BT709;
    frame->color_trc = hdr ? AVCOL_TRC_SMPTE2084 : AVCOL_TRC_BT709;
    frame->colorspace = hdr ? AVCOL_SPC_BT2020_NCL : AVCOL_SPC_BT709;
    return 0;
}

/* name: an encoder name, or decoder device type ("software", "cuda",
 * "vaapi", "qsv", "videotoolbox", optionally "type@device"). output: 0 RGB24,
 * 1 NV12, 2 P010, 3 GPU frame reference (CUDA or VA-API, no download). */
TpCodec *tp_codec_open(int encoder, const char *name, int hevc, int hdr,
                       int width, int height, int fps, int kbps, int output, int gop, int *error) {
    int result = AVERROR(ENOMEM);
    AVDictionary *opts = NULL;
    const AVCodec *implementation = encoder ? avcodec_find_encoder_by_name(name)
        : avcodec_find_decoder(hevc ? AV_CODEC_ID_HEVC : AV_CODEC_ID_H264);
    if (!encoder && !strcmp(name, "qsv"))
        implementation = avcodec_find_decoder_by_name(hevc ? "hevc_qsv" : "h264_qsv");
    TpCodec *codec = calloc(1, sizeof(*codec));
    if (!codec) { *error = result; return NULL; }
    if (!implementation) { result = encoder ? AVERROR_ENCODER_NOT_FOUND : AVERROR_DECODER_NOT_FOUND; goto fail; }
    if ((hdr && !hevc) || (encoder && (!dimensions(width, height) || fps < 1 || fps > 240 || kbps < 1 || kbps > 100000))) {
        result = AVERROR(EINVAL); goto fail;
    }
    codec->ctx = avcodec_alloc_context3(implementation);
    codec->packet = av_packet_alloc();
    codec->frame = av_frame_alloc();
    codec->download = av_frame_alloc();
    codec->converted = av_frame_alloc();
    if (!codec->ctx || !codec->packet || !codec->frame || !codec->download || !codec->converted) goto fail;
    codec->encoder = encoder; codec->hdr = hdr; codec->output_format = output;
    codec->width = width; codec->height = height; codec->fps = fps; codec->kbps = kbps;
    codec->live_bitrate = encoder && (strstr(name, "nvenc") || !strcmp(name, "libx264"));
    codec->hw_format = AV_PIX_FMT_NONE;
    codec->ctx->opaque = codec;
    codec->ctx->thread_count = 4; /* slice threads: encoder emits 4 slices the decoder can parallelise */
    codec->ctx->max_pixels = 33554432;
    codec->ctx->log_level_offset = 8; /* keep warnings/errors, suppress codec info dumps */
    codec->ctx->thread_type = FF_THREAD_SLICE;
    codec->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    codec->ctx->pkt_timebase = (AVRational){1, 1000000000};
    if (encoder) {
        AVCodecContext *ctx = codec->ctx;
        ctx->width = width; ctx->height = height;
        ctx->time_base = (AVRational){1, fps};
        ctx->framerate = (AVRational){fps, 1};
        ctx->bit_rate = (int64_t)kbps * 1000;
        ctx->rc_max_rate = ctx->bit_rate;
        ctx->rc_buffer_size = kbps * 1000 / fps * 2;
        /* gop 0 keeps the historical fps/4 interval; session encoders pass a long
         * interval and force IDRs themselves (periodic or client-requested). */
        ctx->gop_size = gop > 0 ? gop : (fps / 4 > 0 ? fps / 4 : 1);
        ctx->max_b_frames = 0;
        /* Four slices let the client decode each frame in parallel. libx264
         * slices via slice threads; NVENC, VAAPI and QSV read ctx->slices. */
        ctx->slices = 4;
        ctx->flags |= AV_CODEC_FLAG_CLOSED_GOP;
        ctx->color_range = AVCOL_RANGE_MPEG;
        ctx->color_primaries = hdr ? AVCOL_PRI_BT2020 : AVCOL_PRI_BT709;
        ctx->color_trc = hdr ? AVCOL_TRC_SMPTE2084 : AVCOL_TRC_BT709;
        ctx->colorspace = hdr ? AVCOL_SPC_BT2020_NCL : AVCOL_SPC_BT709;
        codec->input_format = hdr ? AV_PIX_FMT_P010LE : AV_PIX_FMT_NV12;
        ctx->pix_fmt = codec->input_format;
        if (strstr(name, "libx")) {
            ctx->pix_fmt = hdr ? AV_PIX_FMT_YUV420P10LE : AV_PIX_FMT_YUV420P;
            av_dict_set(&opts, "preset", "ultrafast", 0);
            av_dict_set(&opts, "tune", "zerolatency", 0);
            av_dict_set(&opts, hevc ? "x265-params" : "x264-params",
                hevc ? "repeat-headers=1:annexb=1:open-gop=0:bframes=0:rc-lookahead=0:pools=1:frame-threads=1:log-level=error"
                     : "repeat-headers=1:annexb=1:bframes=0:rc-lookahead=0:scenecut=0", 0);
            av_dict_set(&opts, "forced-idr", "1", 0); /* requested keyframes are IDR */
        } else if (strstr(name, "nvenc")) {
            av_dict_set(&opts, "preset", "p1", 0);
            av_dict_set(&opts, "tune", "ull", 0);
            av_dict_set(&opts, "zerolatency", "1", 0);
            av_dict_set(&opts, "delay", "0", 0);
            av_dict_set(&opts, "rc", "cbr", 0);
            av_dict_set(&opts, "rc-lookahead", "0", 0);
            av_dict_set(&opts, "forced-idr", "1", 0);
        } else if (strstr(name, "vaapi") || strstr(name, "qsv")) {
            enum AVHWDeviceType type = strstr(name, "vaapi") ? AV_HWDEVICE_TYPE_VAAPI : AV_HWDEVICE_TYPE_QSV;
            result = av_hwdevice_ctx_create(&ctx->hw_device_ctx, type, NULL, NULL, 0);
            if (result < 0) goto fail;
            codec->hw_format = type == AV_HWDEVICE_TYPE_VAAPI ? AV_PIX_FMT_VAAPI : AV_PIX_FMT_QSV;
            ctx->pix_fmt = codec->hw_format;
            ctx->hw_frames_ctx = av_hwframe_ctx_alloc(ctx->hw_device_ctx);
            if (!ctx->hw_frames_ctx) { result = AVERROR(ENOMEM); goto fail; }
            AVHWFramesContext *frames = (void *)ctx->hw_frames_ctx->data;
            frames->format = codec->hw_format;
            frames->sw_format = codec->input_format;
            frames->width = width; frames->height = height; frames->initial_pool_size = 12;
            result = av_hwframe_ctx_init(ctx->hw_frames_ctx);
            if (result < 0) goto fail;
            if (type == AV_HWDEVICE_TYPE_VAAPI)
                av_dict_set(&opts, "rc_mode", "CBR", 0);
            av_dict_set(&opts, "async_depth", "1", 0);
            if (type == AV_HWDEVICE_TYPE_QSV) {
                av_dict_set(&opts, "look_ahead", "0", 0);
                av_dict_set(&opts, "preset", "veryfast", 0);
                av_dict_set(&opts, "forced_idr", "1", 0);
            }
        }
    } else if (strcmp(name, "software")) {
        /* "type@device" opens a specific device, e.g. "vaapi@/dev/dri/renderD128"
         * to decode on the GPU that presents the frames. */
        char type_name[32];
        const char *device = strchr(name, '@');
        size_t type_length = device ? (size_t)(device - name) : strlen(name);
        if (type_length >= sizeof type_name || (device && !device[1])) { result = AVERROR(EINVAL); goto fail; }
        memcpy(type_name, name, type_length);
        type_name[type_length] = '\0';
        if (device) device++;
        enum AVHWDeviceType type = av_hwdevice_find_type_by_name(type_name);
        if (type == AV_HWDEVICE_TYPE_NONE) { result = AVERROR(ENOSYS); goto fail; }
        for (int index = 0;; ++index) {
            const AVCodecHWConfig *config = avcodec_get_hw_config(implementation, index);
            if (!config) { result = AVERROR(ENOSYS); goto fail; }
            if (config->device_type == type && (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
                codec->hw_format = config->pix_fmt; break;
            }
        }
        result = av_hwdevice_ctx_create(&codec->ctx->hw_device_ctx, type, device, NULL, 0);
        if (result < 0) goto fail;
        codec->ctx->get_format = choose_hw;
        /* GPU presentation holds the newest frame until it is drawn. */
        if (output == 3) codec->ctx->extra_hw_frames = 3;
    }
    result = avcodec_open2(codec->ctx, implementation, &opts);
    av_dict_free(&opts);
    if (result < 0) goto fail;
    *error = 0;
    return codec;
fail:
    av_dict_free(&opts);
    tp_codec_free(codec);
    *error = result;
    return NULL;
}

int tp_send_packet(TpCodec *codec, const uint8_t *data, int size, int64_t pts) {
    if (!codec || codec->encoder || size <= 0 || size > 16777216) return AVERROR(EINVAL);
    av_packet_unref(codec->packet);
    int result = av_new_packet(codec->packet, size); /* includes mandatory zero padding */
    if (result < 0) return result;
    memcpy(codec->packet->data, data, size);
    codec->packet->pts = pts;
    codec->packet->dts = pts;
    return avcodec_send_packet(codec->ctx, codec->packet);
}

int tp_send_frame(TpCodec *codec, const uint8_t *data, int size, int64_t pts, int force_key) {
    if (!codec || !codec->encoder) return AVERROR(EINVAL);
    int required = av_image_get_buffer_size(codec->input_format, codec->width, codec->height, 1);
    if (required < 0 || required != size) return AVERROR(EINVAL);
    av_frame_unref(codec->frame);
    AVFrame *frame = codec->frame;
    frame->format = codec->hw_format != AV_PIX_FMT_NONE ? codec->input_format : codec->ctx->pix_fmt;
    frame->width = codec->width; frame->height = codec->height;
    int result = av_frame_get_buffer(frame, 32);
    if (result < 0) return result;
    uint8_t *planes[4]; int strides[4];
    result = av_image_fill_arrays(planes, strides, data, codec->input_format, codec->width, codec->height, 1);
    if (result < 0) return result;
    codec->sws = sws_getCachedContext(codec->sws, codec->width, codec->height, codec->input_format,
        codec->width, codec->height, frame->format, SWS_POINT, NULL, NULL, NULL);
    if (!codec->sws) return AVERROR(ENOMEM);
    result = sws_scale(codec->sws, (const uint8_t *const *)planes, strides, 0, codec->height, frame->data, frame->linesize);
    if (result != codec->height) return AVERROR(EINVAL);
    frame->pts = pts;
    /* Copied to the hardware frame by av_frame_copy_props below. With forced
     * IDR configured, an I picture starts a new closed GOP on every encoder. */
    frame->pict_type = force_key ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    color_frame(frame, codec->hdr);
    if (codec->hw_format != AV_PIX_FMT_NONE) {
        av_frame_unref(codec->download);
        result = av_hwframe_get_buffer(codec->ctx->hw_frames_ctx, codec->download, 0);
        if (result < 0) return result;
        result = av_hwframe_transfer_data(codec->download, frame, 0);
        if (result < 0) return result;
        result = av_frame_copy_props(codec->download, frame);
        if (result < 0) return result;
        frame = codec->download;
    }
    return avcodec_send_frame(codec->ctx, frame);
}

int tp_receive_packet(TpCodec *codec, TpOutput *output) {
    av_packet_unref(codec->packet);
    int result = avcodec_receive_packet(codec->ctx, codec->packet);
    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF) return 0;
    if (result < 0) return result;
    *output = (TpOutput){.data = codec->packet->data, .size = codec->packet->size,
        .key = !!(codec->packet->flags & AV_PKT_FLAG_KEY), .pts = codec->packet->pts};
    return 1;
}

int tp_receive_frame(TpCodec *codec, TpOutput *output) {
    av_frame_unref(codec->frame);
    int result = avcodec_receive_frame(codec->ctx, codec->frame);
    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF) return 0;
    if (result < 0) return result;
    AVFrame *frame = codec->frame;
    if (!dimensions(frame->width, frame->height)) return AVERROR(EINVAL);
    int64_t pts = frame->pts;
    if (codec->output_format == 3) {
        if (frame->format != AV_PIX_FMT_CUDA && frame->format != AV_PIX_FMT_VAAPI) return AVERROR(EINVAL);
        AVFrame *reference = av_frame_clone(frame);
        if (!reference) return AVERROR(ENOMEM);
        *output = (TpOutput){.width = frame->width, .height = frame->height, .pts = pts, .gpu = reference};
        return 1;
    }
    if (codec->hw_format != AV_PIX_FMT_NONE) {
        if (frame->format != codec->hw_format) return AVERROR(EINVAL);
        av_frame_unref(codec->download);
        result = av_hwframe_transfer_data(codec->download, frame, 0);
        if (result < 0) return result;
        result = av_frame_copy_props(codec->download, frame);
        if (result < 0) return result;
        frame = codec->download;
    }
    const AVPixFmtDescriptor *description = av_pix_fmt_desc_get(frame->format);
    if (!description) return AVERROR(EINVAL);
    if (codec->hdr && (description->comp[0].depth < 10 || frame->color_primaries != AVCOL_PRI_BT2020
        || frame->color_trc != AVCOL_TRC_SMPTE2084 || frame->colorspace != AVCOL_SPC_BT2020_NCL)) return AVERROR(EINVAL);
    enum AVPixelFormat target = codec->output_format == 2 ? AV_PIX_FMT_P010LE
        : codec->output_format == 1 ? AV_PIX_FMT_NV12 : AV_PIX_FMT_RGB24;
    av_frame_unref(codec->converted);
    codec->converted->format = target;
    codec->converted->width = frame->width; codec->converted->height = frame->height;
    result = av_frame_get_buffer(codec->converted, 1);
    if (result < 0) return result;
    codec->sws = sws_getCachedContext(codec->sws, frame->width, frame->height, frame->format,
        frame->width, frame->height, target, SWS_POINT, NULL, NULL, NULL);
    if (!codec->sws) return AVERROR(ENOMEM);
    int matrix = frame->colorspace == AVCOL_SPC_BT2020_NCL ? SWS_CS_BT2020
        : frame->colorspace == AVCOL_SPC_BT470BG || frame->colorspace == AVCOL_SPC_SMPTE170M ? SWS_CS_ITU601 : SWS_CS_ITU709;
    sws_setColorspaceDetails(codec->sws, sws_getCoefficients(matrix), frame->color_range == AVCOL_RANGE_JPEG,
        sws_getCoefficients(codec->hdr ? SWS_CS_BT2020 : SWS_CS_ITU709), target == AV_PIX_FMT_RGB24, 0, 1 << 16, 1 << 16);
    result = sws_scale(codec->sws, (const uint8_t *const *)frame->data, frame->linesize,
        0, frame->height, codec->converted->data, codec->converted->linesize);
    if (result != frame->height) return AVERROR(EINVAL);
    int size = av_image_get_buffer_size(target, frame->width, frame->height, 1);
    av_packet_unref(codec->packet);
    result = av_new_packet(codec->packet, size);
    if (result < 0) return result;
    result = av_image_copy_to_buffer(codec->packet->data, size,
        (const uint8_t *const *)codec->converted->data, codec->converted->linesize, target, frame->width, frame->height, 1);
    if (result < 0) return result;
    *output = (TpOutput){.data = codec->packet->data, .size = size,
        .width = frame->width, .height = frame->height, .pts = pts};
    return 1;
}

void tp_codec_flush(TpCodec *codec) { avcodec_flush_buffers(codec->ctx); }

/* NVENC (nvEncReconfigureEncoder) and libx264 (x264_encoder_reconfig) pick up
 * bit_rate / rc_max_rate / rc_buffer_size changes at the next frame, without a
 * new hardware session or forced IDR. Other encoders report ENOSYS so the
 * caller reopens them instead. */
int tp_codec_set_bitrate(TpCodec *codec, int kbps) {
    if (!codec || !codec->encoder || kbps <= 0) return AVERROR(EINVAL);
    if (!codec->live_bitrate) return AVERROR(ENOSYS);
    codec->ctx->bit_rate = (int64_t)kbps * 1000;
    codec->ctx->rc_max_rate = codec->ctx->bit_rate;
    codec->ctx->rc_buffer_size = kbps * 1000 / codec->fps * 2;
    codec->kbps = kbps;
    return 0;
}
void tp_codec_error(int error, char *buffer, int length) { av_strerror(error, buffer, length); }

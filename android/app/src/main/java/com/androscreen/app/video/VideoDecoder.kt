package com.androscreen.app.video

import android.media.MediaCodec
import android.media.MediaCodecInfo.VideoCapabilities
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.view.Surface
import java.nio.ByteBuffer
import kotlin.math.roundToInt

/**
 * Synchronous H.264 decoder that renders straight to a [Surface].
 *
 * One encoded NAL is one access unit: the host encodes with a single slice
 * per frame and no B-frames.
 */
class VideoDecoder(
    surface: Surface,
    width: Int,
    height: Int,
    csd: ByteArray,
    renderHandler: Handler? = null,
    onRendered: ((ptsUs: Long, nanoTime: Long) -> Unit)? = null,
) {
    private val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
    private val bufferInfo = MediaCodec.BufferInfo()

    init {
        try {
            val format = MediaFormat.createVideoFormat(
                MediaFormat.MIMETYPE_VIDEO_AVC,
                width,
                height,
            )
            val (sps, pps) = H264.splitParameterSets(csd)
            when {
                sps != null && pps != null -> {
                    format.setByteBuffer("csd-0", ByteBuffer.wrap(sps))
                    format.setByteBuffer("csd-1", ByteBuffer.wrap(pps))
                }

                csd.isNotEmpty() -> format.setByteBuffer("csd-0", ByteBuffer.wrap(csd))
            }
            // Hold as few decoded pictures as the codec allows. A phone decoder
            // otherwise keeps two or three before it lets the surface have one,
            // and every one of them is a frame of lag the user feels while
            // dragging a window or moving the pointer over the picture.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
            // Realtime priority: a decoder that shares the chip with the rest of
            // the phone must not wait behind ordinary work.
            format.setInteger(MediaFormat.KEY_PRIORITY, 0)
            codec.configure(format, surface, null, 0)
            codec.start()
            if (renderHandler != null && onRendered != null) {
                // The only callback that reports when a picture actually
                // reached the surface; everything else this side measures stops
                // at "handed to the codec".
                codec.setOnFrameRenderedListener(
                    { _, ptsUs, nanos -> onRendered(ptsUs, nanos) },
                    renderHandler,
                )
            }
        } catch (error: Exception) {
            runCatching { codec.release() }
            throw error
        }
    }

    /**
     * Queues one access unit.
     *
     * Returns `false` when the packet had to be dropped because the decoder
     * was still busy. Outputs are drained before waiting again: input buffers
     * stay held until the frames they produced are released from the surface.
     */
    @Throws(IllegalStateException::class)
    fun feed(data: ByteArray, keyframe: Boolean, ptsUs: Long): Boolean {
        var index = codec.dequeueInputBuffer(INPUT_TIMEOUT_US)
        if (index < 0) {
            drain()
            index = codec.dequeueInputBuffer(if (keyframe) KEYFRAME_TIMEOUT_US else INPUT_TIMEOUT_US)
        }
        if (index < 0) return false
        val buffer = codec.getInputBuffer(index)
            ?: throw IllegalStateException("decoder returned no input buffer")
        buffer.clear()
        buffer.put(data)
        val flags = if (keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
        codec.queueInputBuffer(index, 0, data.size, ptsUs, flags)
        return true
    }

    /** Releases every frame that is ready and reports whether one was shown. */
    @Throws(IllegalStateException::class)
    fun drain(): Boolean {
        var rendered = false
        while (true) {
            val index = codec.dequeueOutputBuffer(bufferInfo, 0)
            when {
                index == MediaCodec.INFO_TRY_AGAIN_LATER -> return rendered
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> Unit
                index >= 0 -> {
                    val show = bufferInfo.size > 0
                    codec.releaseOutputBuffer(index, show)
                    rendered = rendered || show
                }

                else -> return rendered
            }
        }
    }

    fun release() {
        runCatching { codec.stop() }
        runCatching { codec.release() }
    }

    private companion object {
        // Two milliseconds: the pipeline now offers a frame, retries the same
        // one next pass when this decoder is full, and drains on every pass.
        // Waiting 20ms per frame here was most of the 50fps ceiling.
        const val INPUT_TIMEOUT_US = 2_000L
        // A freshly rebuilt decoder waits on a keyframe the host emits within a
        // frame or two; half a second here only adds to the turn the user sees.
        const val KEYFRAME_TIMEOUT_US = 50_000L
    }
}

/**
 * The shapes this phone's hardware H.264 decoder will actually accept.
 *
 * A panel can be larger than the decoder's published limits: a 1200x2000
 * tablet is 2.4 megapixels, while its chip promises only 1080p playback.
 * Asking for the panel anyway fails inside `configure`, which the user reads as
 * the app being broken. Asking the codec first lets the request shrink to a
 * shape it will take.
 */
object AvcSupport {
    private val capabilities: VideoCapabilities? by lazy {
        runCatching {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .asSequence()
                .filter { !it.isEncoder }
                .filter { info ->
                    info.supportedTypes.any {
                        it.equals(MediaFormat.MIMETYPE_VIDEO_AVC, ignoreCase = true)
                    }
                }
                .mapNotNull { info ->
                    runCatching {
                        info.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                            .videoCapabilities
                    }.getOrNull()
                }
                .maxByOrNull { it.supportedWidths.upper }
        }.getOrNull()
    }

    /** Whether this decoder accepts a picture of [width] x [height]. */
    private fun supports(width: Int, height: Int): Boolean {
        val caps = capabilities ?: return false
        return runCatching { caps.isSizeSupported(width, height) }.getOrDefault(false)
    }

    /**
     * The largest shape below the ask that the decoder accepts.
     *
     * Returns `null` when even half the asked size does not fit, or when the
     * codec cannot be queried at all, so the caller keeps the request it had
     * rather than guessing at a shape the user never asked for.
     */
    fun fit(width: Int, height: Int): Pair<Int, Int>? {
        if (capabilities == null) return null
        if (supports(width, height)) return width to height
        var scale = 0.98
        while (scale > 0.5) {
            val fittedWidth = ((width * scale).roundToInt() / 2 * 2).coerceAtLeast(320)
            val fittedHeight = ((height * scale).roundToInt() / 2 * 2).coerceAtLeast(320)
            if (supports(fittedWidth, fittedHeight)) return fittedWidth to fittedHeight
            scale -= 0.02
        }
        return null
    }
}

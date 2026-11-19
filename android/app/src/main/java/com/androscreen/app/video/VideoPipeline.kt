package com.androscreen.app.video

import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import java.util.Locale
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/**
 * Owns the decoder and the thread that feeds it.
 *
 * Frames arrive on the USB reader thread; decoding and releasing output
 * buffers happen here so a slow decoder never stalls the link. The decoder is
 * rebuilt when the surface or the stream configuration changes, and playback
 * restarts at the next keyframe.
 *
 * A rebuild is not instant, and the host keeps sending while it happens, so
 * every packet carries the configuration it was written for: a packet from the
 * shape the decoder no longer is gets dropped rather than fed in, which is what
 * used to break the decoder mid-turn.
 */
class VideoPipeline(
    private val onError: (String) -> Unit,
    private val onFirstFrame: () -> Unit,
) {
    private class Packet(
        val data: ByteArray,
        val keyframe: Boolean,
        val ptsUs: Long,
        val epoch: Int,
        /** When the reader handed it over, for the timing trace only. */
        val queuedAtNs: Long,
    )

    private data class Config(val width: Int, val height: Int, val fps: Int, val csd: ByteArray)

    private val lock = Any()

    // Two frames: the one being decoded and the newest one waiting. Anything
    // older is already superseded by what came after it, and keeping it only
    // adds latency the user feels.
    private val queue = LinkedBlockingQueue<Packet>(2)
    private var surface: Surface? = null
    private var config: Config? = null
    private var epoch = 0
    private var frameIndex = 0L
    private var reported = false
    private var failures = 0

    /**
     * Set when a frame was thrown away, so the next picture that decodes is
     * a keyframe.
     *
     * Every P frame is a reference for the ones after it, so dropping one
     * without telling the loop makes the decoder paint from a picture that
     * never arrived — visible as garbage until the host's next IDR.
     */
    @Volatile
    private var awaitingKeyframe = true

    /** Counters behind [TRACE]; only touched while it is on. */
    private val traceFed = AtomicLong()
    private val traceShown = AtomicLong()
    private val traceDropped = AtomicLong()
    private val traceWaitNs = AtomicLong()
    private val traceWaitMaxNs = AtomicLong()
    private val traceFeedNs = AtomicLong()
    private val traceFeedMaxNs = AtomicLong()
    private var traceWindowNs = SystemClock.elapsedRealtimeNanos()

    /**
     * When each picture reached the link, keyed by the PTS that names it, so the
     * render callback can say how long this side held it.
     *
     * This is the only number on this side that covers the whole phone half of
     * the path — USB receive, queue, decode, and SurfaceFlinger picking the
     * picture up — and it is taken entirely on one clock.
     */
    private val traceReceived = ConcurrentHashMap<Long, Long>()
    private val traceRendered = AtomicLong()
    private val traceRenderNs = AtomicLong()
    private val traceRenderMaxNs = AtomicLong()

    /** Delivers render callbacks off the main thread, so the numbers are the path and not the UI. */
    private val traceHandler: Handler? =
        if (TRACE) Handler(HandlerThread("androscreen-latency").apply { start() }.looper) else null

    @Volatile
    private var running = true

    private val worker = Thread({ loop() }, "androscreen-video")

    init {
        worker.start()
    }

    fun setSurface(surface: Surface?) {
        synchronized(lock) {
            if (this.surface === surface) return
            this.surface = surface
            epoch++
            queue.clear()
            traceReceived.clear()
        }
    }

    fun configure(width: Int, height: Int, fps: Int, csd: ByteArray) {
        synchronized(lock) {
            val wanted = Config(width, height, fps.coerceAtLeast(1), csd)
            val current = config
            // The same description twice is not a new stream. Rebuilding on it
            // would throw away the keyframe that arrived behind the first copy,
            // which is the one picture the new decoder needs to start on.
            if (current != null &&
                current.width == wanted.width &&
                current.height == wanted.height &&
                current.fps == wanted.fps &&
                current.csd.contentEquals(wanted.csd)
            ) {
                return
            }
            config = wanted
            frameIndex = 0
            epoch++
            queue.clear()
            // A new stream numbers its frames from zero again, so the receive
            // times of the old one would answer for the wrong pictures.
            traceReceived.clear()
        }
        // The worker parks in queue.poll() while the stream is quiet. Waking it
        // lets a turn rebuild its decoder on this pass instead of after the
        // poll's timeout, which the user sees as a long frozen picture.
        worker.interrupt()
    }

    fun submit(data: ByteArray, keyframe: Boolean) {
        val packet = synchronized(lock) {
            if (!running) return
            val fps = config?.fps ?: 60
            frameIndex++
            Packet(
                data,
                keyframe,
                frameIndex * 1_000_000L / fps,
                epoch,
                SystemClock.elapsedRealtimeNanos(),
            )
        }
        if (!queue.offer(packet)) {
            queue.poll()
            queue.offer(packet)
            // The frame that just went out is a reference the decoder will
            // never get, so it has to start again from a keyframe.
            awaitingKeyframe = true
            if (TRACE) traceDropped.incrementAndGet()
        } else if (TRACE) {
            // Stamped after the frame is in hand: everything measured from here
            // is this side's own work.
            if (traceReceived.size > RECEIVED_CAP) traceReceived.clear()
            traceReceived[packet.ptsUs] = System.nanoTime()
        }
    }

    fun close() {
        running = false
        synchronized(lock) {
            surface = null
            config = null
            epoch++
            queue.clear()
        }
        worker.interrupt()
    }

    private fun loop() {
        var decoder: VideoDecoder? = null
        var appliedEpoch = -1
        var needKeyframe = true
        var frameShown = false
        var pending: Packet? = null

        while (running) {
            if (pending != null) pauseBeforeRetry()
            // Take a backlog without waiting and only park on an empty queue.
            // One timed poll per frame capped the stream at the poll rate, well
            // below the rate the host sends.
            val packet = pending ?: queue.poll() ?: try {
                queue.poll(POLL_MS, TimeUnit.MILLISECONDS)
            } catch (_: InterruptedException) {
                null
            }
            pending = null

            val next = synchronized(lock) {
                if (epoch != appliedEpoch) Triple(epoch, surface, config) else null
            }
            if (next != null) {
                appliedEpoch = next.first
                decoder?.release()
                decoder = null
                needKeyframe = true
                awaitingKeyframe = true
                frameShown = false
                val (_, targetSurface, targetConfig) = next
                if (targetSurface != null && targetSurface.isValid && targetConfig != null) {
                    decoder = try {
                        VideoDecoder(
                            targetSurface,
                            targetConfig.width,
                            targetConfig.height,
                            targetConfig.csd,
                            traceHandler,
                            if (TRACE) ::noteRendered else null,
                        )
                    } catch (error: Exception) {
                        report("无法创建视频解码器：${error.message ?: "未知错误"}")
                        null
                    }
                }
            }

            // A frame the queue had to throw away is a reference this decoder
            // will never see; whatever comes next has to start a fresh picture.
            if (awaitingKeyframe) needKeyframe = true

            // Outputs must be released even while the packet queue is empty;
            // otherwise the decoder's input buffers never come back.
            val active = decoder
            if (active != null) {
                try {
                    if (active.drain()) {
                        failures = 0
                        reported = false
                        if (!frameShown) {
                            frameShown = true
                            onFirstFrame()
                        }
                        if (TRACE) traceShown.incrementAndGet()
                    }
                } catch (error: Exception) {
                    decoder = null
                    appliedEpoch = -1
                    needKeyframe = true
                    awaitingKeyframe = true
                    discard(active, error)
                    continue
                }
            }

            if (packet == null) continue
            // Written for a shape this decoder is not, so it is not for it.
            if (packet.epoch != appliedEpoch) continue
            val current = decoder ?: continue
            if (needKeyframe && !packet.keyframe) continue

            try {
                val startedNs = SystemClock.elapsedRealtimeNanos()
                if (!current.feed(packet.data, packet.keyframe, packet.ptsUs)) {
                    // The decoder was busy, not broken: hold the frame over to
                    // the next pass instead of losing the keyframe it waited
                    // for and starting the wait again.
                    pending = packet
                    continue
                }
                if (TRACE) noteFed(packet, startedNs)
                needKeyframe = false
                if (packet.keyframe) awaitingKeyframe = false
            } catch (error: Exception) {
                decoder = null
                appliedEpoch = -1
                needKeyframe = true
                awaitingKeyframe = true
                discard(current, error)
            }
        }
        decoder?.release()
    }

    /**
     * Waits a moment before retrying a frame the decoder refused.
     *
     * Retrying straight away would spin the worker while the decoder has no
     * input buffer to give back.
     */
    private fun pauseBeforeRetry() {
        try {
            Thread.sleep(RETRY_PAUSE_MS)
        } catch (_: InterruptedException) {
            // close() interrupts the pause; the loop condition ends it.
        }
    }

    /**
     * Records one picture's trip from the link to the display surface.
     *
     * Called on the latency handler rather than the decode thread, so the number
     * is the path and not this thread's queue. The callback's own timestamp
     * comes from the composer's clock, which is not the one this side stamps
     * frames with, so the moment of the call is used instead.
     */
    private fun noteRendered(ptsUs: Long, @Suppress("UNUSED_PARAMETER") nanos: Long) {
        val received = traceReceived.remove(ptsUs) ?: return
        val elapsed = System.nanoTime() - received
        traceRendered.incrementAndGet()
        traceRenderNs.addAndGet(elapsed)
        traceRenderMaxNs.accumulateAndGet(elapsed) { a, b -> if (a > b) a else b }
    }

    /** Adds one accepted frame to the per-second timing trace. */
    private fun noteFed(packet: Packet, startedNs: Long) {
        val now = SystemClock.elapsedRealtimeNanos()
        val waited = startedNs - packet.queuedAtNs
        val fed = now - startedNs
        traceFed.incrementAndGet()
        traceWaitNs.addAndGet(waited)
        traceWaitMaxNs.accumulateAndGet(waited) { a, b -> if (a > b) a else b }
        traceFeedNs.addAndGet(fed)
        traceFeedMaxNs.accumulateAndGet(fed) { a, b -> if (a > b) a else b }
        reportTrace(now)
    }

    /**
     * Prints what the last second of the decode path did, when tracing is on.
     *
     * The queue wait is the part of the latency this side owns: it covers the
     * hand-off, the queue, and any decoder rebuild.
     */
    private fun reportTrace(now: Long) {
        if (!TRACE) return
        val elapsed = now - traceWindowNs
        if (elapsed < TRACE_PERIOD_NS) return
        val seconds = elapsed / 1e9
        val fed = traceFed.getAndSet(0)
        val shown = traceShown.getAndSet(0)
        val dropped = traceDropped.getAndSet(0)
        val waitNs = traceWaitNs.getAndSet(0)
        val waitMax = traceWaitMaxNs.getAndSet(0)
        val feedNs = traceFeedNs.getAndSet(0)
        val feedMax = traceFeedMaxNs.getAndSet(0)
        val rendered = traceRendered.getAndSet(0)
        val renderNs = traceRenderNs.getAndSet(0)
        val renderMax = traceRenderMaxNs.getAndSet(0)
        traceWindowNs = now
        Log.i(
            TAG,
            String.format(
                Locale.US,
                "stream %.1f fps, shown %d, dropped %d, queue %d," +
                    " wait %.1fms (max %.1fms), feed %.1fms (max %.1fms)," +
                    " render %d, receive→render %.1fms (max %.1fms)",
                fed / seconds,
                shown,
                dropped,
                queue.size,
                waitNs / 1e6 / fed.coerceAtLeast(1),
                waitMax / 1e6,
                feedNs / 1e6 / fed.coerceAtLeast(1),
                feedMax / 1e6,
                rendered,
                renderNs / 1e6 / rendered.coerceAtLeast(1),
                renderMax / 1e6,
            ),
        )
    }

    /**
     * Throws away a decoder that stopped making sense and builds it again at
     * the next keyframe.
     *
     * The host prints a keyframe every couple of seconds, so one broken picture
     * costs a short freeze and nothing else; it is a run of them that the user
     * needs to hear about.
     */
    private fun discard(broken: VideoDecoder, error: Exception) {
        broken.release()
        failures++
        Log.w(TAG, "decoder failure $failures: ${error.message ?: "未知错误"}")
        if (failures >= MAX_FAILURES) report("视频解码中断：${error.message ?: "未知错误"}")
    }

    private fun report(message: String) {
        if (!running) return
        if (reported) return
        reported = true
        Log.w(TAG, message)
        onError(message)
    }

    private companion object {
        const val TAG = "VideoPipeline"

        /** Broken pictures in a row before the stream is called broken. */
        const val MAX_FAILURES = 3

        /** How long the worker parks when the queue is empty, in milliseconds. */
        const val POLL_MS = 20L

        /** Pause before retrying a frame the decoder refused, in milliseconds. */
        const val RETRY_PAUSE_MS = 1L

        /**
         * Reports the decode path's per-second timings.
         *
         * Off by default: a packaged build has no business logging frames.
         * Turn it on while measuring and rebuild.
         */
        const val TRACE = false

        /** Receive times kept while waiting for their render callback. */
        const val RECEIVED_CAP = 512

        const val TRACE_PERIOD_NS = 1_000_000_000L
    }
}

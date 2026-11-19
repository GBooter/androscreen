package com.androscreen.app.audio

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.util.Log
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlin.math.max

/**
 * Plays the host's PCM stream through an [AudioTrack].
 *
 * Chunks are queued so USB reads never block on audio latency; when the phone
 * falls behind, the oldest chunks are dropped instead of accumulating delay.
 * Writes are non-blocking so closing the screen never waits on the audio
 * device.
 */
class AudioPlayer {
    private val queue = LinkedBlockingQueue<ByteArray>(QUEUE_CHUNKS)
    private val lock = Any()

    @Volatile
    private var running = true

    private var sampleRate = 48_000
    private var channels = 2
    private var track: AudioTrack? = null

    private val worker = Thread({ loop() }, "androscreen-audio")

    init {
        worker.start()
    }

    fun configure(sampleRate: Int, channels: Int) {
        synchronized(lock) {
            if (track == null) {
                this.sampleRate = sampleRate
                this.channels = channels
            }
        }
    }

    fun submit(chunk: ByteArray) {
        if (!running) return
        if (!queue.offer(chunk)) {
            queue.poll()
            queue.offer(chunk)
        }
    }

    fun close() {
        running = false
        queue.clear()
        worker.interrupt()
    }

    private fun loop() {
        var pending: ByteArray? = null
        var offset = 0

        while (running) {
            val chunk = pending ?: try {
                queue.poll(200, TimeUnit.MILLISECONDS)
            } catch (_: InterruptedException) {
                null
            }
            if (chunk == null) continue
            pending = chunk

            val output = ensureTrack()
            if (output == null) {
                try {
                    Thread.sleep(TRACK_RETRY_MS)
                } catch (_: InterruptedException) {
                    break
                }
                continue
            }
            val written = try {
                output.write(chunk, offset, chunk.size - offset, AudioTrack.WRITE_NON_BLOCKING)
            } catch (error: Exception) {
                Log.d(TAG, "audio output stopped: ${error.message}")
                break
            }
            when {
                written < 0 -> break
                written == 0 -> try {
                    Thread.sleep(BACKOFF_MS)
                } catch (_: InterruptedException) {
                    break
                }

                else -> {
                    offset += written
                    if (offset >= chunk.size) {
                        pending = null
                        offset = 0
                    }
                }
            }
        }

        val playing = synchronized(lock) {
            track.also { track = null }
        }
        if (playing != null) {
            runCatching { playing.stop() }
            runCatching { playing.flush() }
            runCatching { playing.release() }
        }
    }

    private fun ensureTrack(): AudioTrack? = synchronized(lock) {
        track ?: buildTrack()?.also {
            track = it
            it.play()
        }
    }

    private fun buildTrack(): AudioTrack? = try {
        val channelMask = if (channels == 1) {
            AudioFormat.CHANNEL_OUT_MONO
        } else {
            AudioFormat.CHANNEL_OUT_STEREO
        }
        val minimum = AudioTrack.getMinBufferSize(
            sampleRate,
            channelMask,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        val targetFrames = BUFFER_CHUNKS * FRAMES_PER_CHUNK
        val bufferSize = max(minimum, targetFrames * max(channels, 1) * 2)
        val built = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                    .build(),
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(sampleRate)
                    .setChannelMask(channelMask)
                    .build(),
            )
            // The default mode routed this stream through the deep buffer, and
            // the picture leads the sound by everything that buffer cost. The
            // phone is a display extension, so audio has to stay on the short
            // path even though a fast track leaves less room for USB jitter.
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setBufferSizeInBytes(bufferSize)
            .build()
        // The requested size is a hint the system may round up; what the
        // listener hears is the buffered audio, so trim it back afterwards.
        runCatching { built.setBufferSizeInFrames(targetFrames) }
        built
    } catch (error: Exception) {
        Log.w(TAG, "cannot create audio track: ${error.message}")
        null
    }

    private companion object {
        const val TAG = "AudioPlayer"
        const val BACKOFF_MS = 5L
        const val TRACK_RETRY_MS = 100L
        /** Frames in one host chunk, matching the 20 ms slice on the wire. */
        const val FRAMES_PER_CHUNK = 960
        /** Chunks the track holds ahead of playback: 40 ms of margin. */
        const val BUFFER_CHUNKS = 2
        /** Chunks queued for the writer; anything older is already late. */
        const val QUEUE_CHUNKS = 8
    }
}

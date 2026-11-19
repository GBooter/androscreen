package com.androscreen.app.link

import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import android.util.Log
import com.androscreen.app.protocol.Frame
import com.androscreen.app.protocol.FrameReader
import com.androscreen.app.protocol.Wire
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

/**
 * Protocol transport over an open AOA accessory file descriptor.
 *
 * A reader thread parses incoming frames, a writer thread drains outgoing
 * frames. Both ends fail quietly: an unplugged cable just closes the link.
 */
class UsbLink(
    pfd: ParcelFileDescriptor,
    private val listener: Listener,
) {
    interface Listener {
        fun onControl(payload: ByteArray)
        fun onVideo(frame: Frame)
        fun onAudio(payload: ByteArray)
        fun onCursor(payload: ByteArray)
        fun onPong(awaitingHello: Boolean)
        fun onClosed()
    }

    private val descriptor = pfd
    private val input = FileInputStream(pfd.fileDescriptor)
    private val frames = FrameReader(input)
    private val output = FileOutputStream(pfd.fileDescriptor)
    private val outgoing = LinkedBlockingQueue<Pending>(64)
    private val closedOnce = AtomicBoolean(false)
    private val dropped = AtomicLong()

    @Volatile
    private var running = true

    private val reader = Thread({ readLoop() }, "androscreen-usb-reader")
    private val writer = Thread({ writeLoop() }, "androscreen-usb-writer")

    /** When the host last spoke, used to notice a daemon that restarted silently. */
    @Volatile
    var lastFrameAt: Long = SystemClock.elapsedRealtime()
        private set

    init {
        reader.start()
        writer.start()
    }

    fun sendControl(payload: ByteArray) {
        enqueue(Wire.KIND_CONTROL, payload)
    }

    fun sendTouch(payload: ByteArray) {
        enqueue(Wire.KIND_TOUCH, payload)
    }

    fun close() {
        shutdown(null)
    }

    private fun enqueue(kind: Int, payload: ByteArray) {
        if (!running) return
        val pending = Pending(Wire.encode(kind, payload), kind, SystemClock.elapsedRealtime())
        if (outgoing.offer(pending)) return

        // The writer is behind, and everything this queue carries is upstream
        // control the host acts on. A touch report is one position in a stream
        // the next report supersedes, and a pong only answers a liveness
        // question, so those are what make room: dropping a greeting, a start,
        // or a release instead left the phone waiting on a command that was
        // never going to arrive.
        if (evict(Wire.KIND_TOUCH) || evict(Wire.KIND_PONG)) {
            noteDropped()
            if (outgoing.offer(pending)) return
        }
        // Only commands were queued, so one is lost either way. Keep the
        // newest: it carries the latest intent, such as the turn the user just
        // asked for, and the host re-sends greetings on its own.
        noteDropped()
        outgoing.poll()
        outgoing.offer(pending)
    }

    /**
     * Removes the oldest queued frame of one kind, if there is one.
     *
     * The iteration is weakly consistent, so a frame the writer takes at the
     * same moment can make this report nothing; the caller falls back to the
     * plain eviction in that case.
     */
    private fun evict(kind: Int): Boolean {
        val iterator = outgoing.iterator()
        while (iterator.hasNext()) {
            val candidate = iterator.next()
            if (candidate.kind == kind) {
                iterator.remove()
                return true
            }
        }
        return false
    }

    private fun noteDropped() {
        val lost = dropped.incrementAndGet()
        if (lost == 1L || lost % DROPPED_LOG_EVERY == 0L) {
            Log.w(TAG, "the writer is stuck; dropped $lost upstream frames")
        }
    }

    private fun readLoop() {
        try {
            while (true) {
                val frame = frames.next() ?: break
                lastFrameAt = SystemClock.elapsedRealtime()
                when (frame.kind) {
                    Wire.KIND_CONTROL -> listener.onControl(frame.payload)
                    Wire.KIND_VIDEO -> listener.onVideo(frame)
                    Wire.KIND_AUDIO -> listener.onAudio(frame.payload)
                    Wire.KIND_CURSOR -> listener.onCursor(frame.payload)
                    Wire.KIND_PING -> enqueue(Wire.KIND_PONG, ByteArray(0))
                    Wire.KIND_PONG -> listener.onPong(frame.awaitingHello)
                    else -> Unit
                }
            }
            shutdown(null)
        } catch (error: IOException) {
            shutdown(error)
        } catch (error: RuntimeException) {
            shutdown(error)
        }
    }

    private fun writeLoop() {
        val ping = Wire.encode(Wire.KIND_PING, ByteArray(0))
        try {
            while (running) {
                val pending = outgoing.poll(FLUSH_BEAT_MS, TimeUnit.MILLISECONDS)
                if (pending != null) {
                    val waited = SystemClock.elapsedRealtime() - pending.queuedAt
                    val before = SystemClock.elapsedRealtime()
                    writeFully(pending.bytes)
                    val took = SystemClock.elapsedRealtime() - before
                    if (waited > SLOW_MS || took > SLOW_MS) {
                        Log.w(
                            TAG,
                            "kind ${pending.kind} of ${pending.bytes.size}B waited ${waited}ms," +
                                " wrote in ${took}ms",
                        )
                    }
                } else {
                    // The accessory driver releases a queued frame only when a
                    // later write follows it, so a quiet link must keep beating.
                    writeFully(ping)
                }
            }
        } catch (_: InterruptedException) {
            // close() interrupted the wait; the loop condition ends it.
        } catch (error: IOException) {
            shutdown(error)
        }
    }

    /**
     * Pushes one frame out, repeating whatever the kernel did not accept.
     *
     * `FileOutputStream.write` ignores a short write, and the accessory driver
     * answers with one whenever its queue is full. Losing the tail of a frame
     * desynchronises the whole stream: the host then reads the next frame's
     * first bytes as the end of this one.
     */
    private fun writeFully(bytes: ByteArray) {
        var offset = 0
        var attempts = 0
        while (offset < bytes.size) {
            val before = SystemClock.elapsedRealtime()
            val written = try {
                Os.write(descriptor.fileDescriptor, bytes, offset, bytes.size - offset)
            } catch (error: ErrnoException) {
                // A full queue answers EAGAIN, and a phone busy turning answers
                // EINTR/EBUSY: all three mean "not now", not "gone". Retrying is
                // only safe while this frame has put nothing on the wire; past
                // that, resending the head would splice a duplicate into the
                // stream and desynchronise the host for good.
                val retryable = offset == 0 && attempts < WRITE_ATTEMPTS &&
                    (error.errno == OsConstants.EAGAIN ||
                        error.errno == OsConstants.EINTR ||
                        error.errno == OsConstants.EBUSY)
                if (retryable) {
                    attempts += 1
                    Log.d(TAG, "write not accepted (${error.message}); retry $attempts")
                    Thread.sleep(WRITE_RETRY_DELAY_MS)
                    continue
                }
                Log.w(TAG, "write of ${bytes.size - offset} bytes failed: ${error.message}")
                throw IOException(error.message, error)
            }
            val slow = SystemClock.elapsedRealtime() - before
            if (slow > SLOW_MS) {
                Log.w(TAG, "$written of ${bytes.size} bytes took ${slow}ms")
            }
            if (written <= 0) {
                throw IOException("the accessory link accepted no more bytes")
            }
            offset += written
        }
    }

    private fun shutdown(cause: Throwable?) {
        if (!running && closedOnce.get()) {
            Log.d(TAG, "link already closed: ${cause?.message}")
            return
        }
        Log.d(TAG, "shutting down: ${cause?.message ?: "peer closed the link"}")
        running = false
        outgoing.clear()
        writer.interrupt()
        runCatching { input.close() }
        runCatching { output.close() }
        runCatching { descriptor.close() }
        if (closedOnce.compareAndSet(false, true)) {
            if (cause != null) Log.d(TAG, "link closed: ${cause.message}")
            listener.onClosed()
        }
    }

    private companion object {
        const val TAG = "UsbLink"

        /** How often an idle link beats, in milliseconds. */
        const val FLUSH_BEAT_MS = 200L

        /** How many times a refused frame write is retried before giving up. */
        const val WRITE_ATTEMPTS = 3

        /** Pause between retries of a write the kernel refused, in milliseconds. */
        const val WRITE_RETRY_DELAY_MS = 2L

        /** Anything slower than this on the control path is worth a log line. */
        const val SLOW_MS = 200L

        /** Throttle for the "writer is stuck" warning, in dropped frames. */
        const val DROPPED_LOG_EVERY = 100L
    }
}

/** One encoded frame waiting for the writer thread, stamped when it was queued. */
private class Pending(val bytes: ByteArray, val kind: Int, val queuedAt: Long)

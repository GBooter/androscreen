package com.androscreen.app.protocol

import java.io.EOFException
import java.io.IOException
import java.io.InputStream

/**
 * Buffered frame decoder for the accessory endpoint.
 *
 * That endpoint hands over data one USB receive request at a time and drops
 * whatever a short read leaves behind, so the stream must be collected with
 * large reads and decoded from the accumulated buffer. Scanning for the magic
 * inside the buffer also recovers from bytes left over by an earlier session.
 */
class FrameReader(private val input: InputStream) {
    private var data = ByteArray(INITIAL_CAPACITY)
    private var size = 0

    @Throws(IOException::class)
    fun next(): Frame? {
        while (true) {
            val frame = parse()
            if (frame != null) return frame
            if (size == data.size) {
                if (data.size >= MAX_FRAME) throw IOException("frame exceeds the payload limit")
                data = data.copyOf(minOf(data.size * 2, MAX_FRAME))
            }
            val count = input.read(data, size, data.size - size)
            if (count < 0) {
                if (size == 0) return null
                throw EOFException("link closed mid-frame")
            }
            size += count
        }
    }

    private fun parse(): Frame? {
        while (size >= HEADER_LEN) {
            if (!startsWithMagic()) {
                val next = indexOfMagic()
                if (next < 0) {
                    // Keep a possible partial magic for the next read.
                    discard(size - minOf(size, MAGIC.size - 1))
                    return null
                }
                discard(next)
                continue
            }
            if ((data[4].toInt() and 0xFF) != Wire.PROTOCOL_VERSION) {
                discard(4)
                continue
            }
            val kind = data[5].toInt() and 0xFF
            if (kind !in Wire.KIND_CONTROL..Wire.KIND_CURSOR) {
                discard(4)
                continue
            }
            val length = (data[8].toInt() and 0xFF) or
                ((data[9].toInt() and 0xFF) shl 8) or
                ((data[10].toInt() and 0xFF) shl 16) or
                ((data[11].toInt() and 0xFF) shl 24)
            val limit = if (kind == Wire.KIND_VIDEO) MAX_PAYLOAD else MAX_SMALL_PAYLOAD
            if (length < 0 || length > limit) {
                discard(4)
                continue
            }
            if (size < HEADER_LEN + length) return null
            val flags = data[6].toInt() and 0xFF
            val payload = data.copyOfRange(HEADER_LEN, HEADER_LEN + length)
            discard(HEADER_LEN + length)
            return Frame(kind, flags, payload)
        }
        return null
    }

    private fun startsWithMagic(): Boolean {
        for (index in MAGIC.indices) {
            if (data[index] != MAGIC[index]) return false
        }
        return true
    }

    /** Index of the next magic occurrence, or -1 when there is none. */
    private fun indexOfMagic(): Int {
        var index = 1
        while (index + MAGIC.size <= size) {
            if (data[index] == MAGIC[0] &&
                data[index + 1] == MAGIC[1] &&
                data[index + 2] == MAGIC[2] &&
                data[index + 3] == MAGIC[3]
            ) {
                return index
            }
            index++
        }
        return -1
    }

    private fun discard(count: Int) {
        if (count >= size) {
            size = 0
            return
        }
        System.arraycopy(data, count, data, 0, size - count)
        size -= count
    }

    private companion object {
        const val HEADER_LEN = 12
        const val MAX_PAYLOAD = 8 * 1024 * 1024
        /** Control, touch and audio frames never grow anywhere near this. */
        const val MAX_SMALL_PAYLOAD = 1024 * 1024
        const val MAX_FRAME = MAX_PAYLOAD + HEADER_LEN
        const val INITIAL_CAPACITY = 256 * 1024
        val MAGIC = byteArrayOf(
            'A'.code.toByte(),
            'S'.code.toByte(),
            'C'.code.toByte(),
            'R'.code.toByte(),
        )
    }
}

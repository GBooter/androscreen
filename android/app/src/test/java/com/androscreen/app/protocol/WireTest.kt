package com.androscreen.app.protocol

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.EOFException
import java.io.InputStream

class WireTest {
    @Test
    fun roundTripPreservesFrame() {
        val payload = byteArrayOf(9, 8, 7, 6)
        val frame = reader(Wire.encode(Wire.KIND_VIDEO, payload, Wire.FLAG_KEYFRAME)).next()!!

        assertEquals(Wire.KIND_VIDEO, frame.kind)
        assertTrue(frame.isKeyframe)
        assertArrayEquals(payload, frame.payload)
    }

    @Test
    fun plainFrameIsNotKeyframe() {
        val frame = reader(Wire.encode(Wire.KIND_AUDIO, byteArrayOf(1))).next()!!
        assertFalse(frame.isKeyframe)
    }

    @Test
    fun closedLinkEndsTheStream() {
        assertNull(reader(ByteArray(0)).next())
    }

    @Test
    fun linkClosedMidFrameThrows() {
        val header = header(kind = Wire.KIND_VIDEO, length = 4)
        assertThrows(EOFException::class.java) {
            reader(header).next()
        }
    }

    @Test
    fun aCursorFrameSurvivesTheReader() {
        val payload = byteArrayOf(1, 1, 2, 0, 1, 0, 1, 0, 0, 0, 0x80.toByte(), 0x0c, 0xd0.toByte(), 0x07)
        val frame = reader(Wire.encode(Wire.KIND_CURSOR, payload)).next()!!

        assertEquals(Wire.KIND_CURSOR, frame.kind)
        assertArrayEquals(payload, frame.payload)
    }

    @Test
    fun framesShareOneBuffer() {
        val stream = Wire.encode(Wire.KIND_CONTROL, byteArrayOf(1)) +
            Wire.encode(Wire.KIND_TOUCH, byteArrayOf(2, 3))
        val source = reader(stream)
        assertEquals(Wire.KIND_CONTROL, source.next()!!.kind)
        assertEquals(Wire.KIND_TOUCH, source.next()!!.kind)
    }

    @Test
    fun bytesLeftByAnEarlierSessionAreSkipped() {
        val stream = byteArrayOf(7, 'S'.code.toByte(), 'C'.code.toByte()) +
            Wire.encode(Wire.KIND_PONG, byteArrayOf(1))
        val frame = reader(stream).next()!!
        assertEquals(Wire.KIND_PONG, frame.kind)
        assertTrue("an empty ping tells the phone to greet again", frame.awaitingHello)
    }

    @Test
    fun anotherProtocolVersionIsResyncedPast() {
        val header = header(kind = Wire.KIND_CONTROL, length = 0)
        header[4] = (Wire.PROTOCOL_VERSION + 1).toByte()
        val frame = reader(header + Wire.encode(Wire.KIND_PONG, ByteArray(0))).next()!!
        assertEquals(Wire.KIND_PONG, frame.kind)
    }

    @Test
    fun anUnknownKindIsResyncedPast() {
        val header = header(kind = Wire.KIND_CONTROL, length = 0)
        header[5] = 99
        val frame = reader(header + Wire.encode(Wire.KIND_PONG, ByteArray(0))).next()!!
        assertEquals(Wire.KIND_PONG, frame.kind)
    }

    @Test
    fun anOversizedPayloadIsResyncedPast() {
        val header = header(kind = Wire.KIND_VIDEO, length = 8 * 1024 * 1024 + 1)
        val frame = reader(header + Wire.encode(Wire.KIND_PONG, ByteArray(0))).next()!!
        assertEquals(Wire.KIND_PONG, frame.kind)
    }

    private fun reader(bytes: ByteArray): FrameReader = FrameReader(bytes.asStream())

    /**
     * Reads that hand back one byte at a time, so a frame is only ever
     * assembled from the bytes still to come.
     */
    private fun ByteArray.asStream(): InputStream = object : InputStream() {
        private var at = 0

        override fun read(): Int = if (at < size) (this@asStream[at++].toInt() and 0xFF) else -1

        override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
            if (at >= size) return -1
            val count = minOf(length, size - at)
            this@asStream.copyInto(buffer, offset, at, at + count)
            at += count
            return count
        }
    }

    private fun header(kind: Int, length: Int): ByteArray {
        val bytes = ByteArray(12)
        bytes[0] = 'A'.code.toByte()
        bytes[1] = 'S'.code.toByte()
        bytes[2] = 'C'.code.toByte()
        bytes[3] = 'R'.code.toByte()
        bytes[4] = Wire.PROTOCOL_VERSION.toByte()
        bytes[5] = kind.toByte()
        bytes[8] = (length and 0xFF).toByte()
        bytes[9] = ((length ushr 8) and 0xFF).toByte()
        bytes[10] = ((length ushr 16) and 0xFF).toByte()
        bytes[11] = ((length ushr 24) and 0xFF).toByte()
        return bytes
    }
}

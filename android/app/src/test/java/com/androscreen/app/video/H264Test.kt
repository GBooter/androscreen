package com.androscreen.app.video

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class H264Test {
    private val sps = byteArrayOf(0, 0, 1, 0x67.toByte(), 0x42, 0x00, 0x1F)
    private val pps = byteArrayOf(
        0,
        0,
        1,
        0x68.toByte(),
        0xCE.toByte(),
        0x3C,
        0x80.toByte(),
    )

    @Test
    fun splitsCsdIntoParameterSets() {
        val (splitSps, splitPps) = H264.splitParameterSets(sps + pps)
        assertArrayEquals(sps, splitSps)
        assertArrayEquals(pps, splitPps)
    }

    @Test
    fun reportsNalTypes() {
        assertEquals(7, H264.nalType(sps))
        assertEquals(8, H264.nalType(pps))
        assertEquals(5, H264.nalType(byteArrayOf(0, 0, 1, 0x65, 1)))
    }

    @Test
    fun emptyCsdYieldsNoParameterSets() {
        val (splitSps, splitPps) = H264.splitParameterSets(ByteArray(0))
        assertNull(splitSps)
        assertNull(splitPps)
    }

    @Test
    fun handlesFourByteStartCodes() {
        val fourByteSps = byteArrayOf(0, 0, 0, 1, 0x67, 0x42)
        val (split, _) = H264.splitParameterSets(fourByteSps)
        assertArrayEquals(
            byteArrayOf(0, 0, 1, 0x67.toByte(), 0x42),
            split,
        )
    }
}

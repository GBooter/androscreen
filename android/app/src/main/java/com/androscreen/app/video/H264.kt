package com.androscreen.app.video

/** Helpers for the Annex-B stream the host sends. */
object H264 {
    private const val NAL_SPS = 7
    private const val NAL_PPS = 8

    /** Returns the first [sps, pps] pair found in [csd], each with its start code. */
    fun splitParameterSets(csd: ByteArray): Pair<ByteArray?, ByteArray?> {
        var sps: ByteArray? = null
        var pps: ByteArray? = null
        for (nal in nals(csd)) {
            when (nalType(nal)) {
                NAL_SPS -> if (sps == null) sps = nal
                NAL_PPS -> if (pps == null) pps = nal
            }
        }
        return sps to pps
    }

    fun nalType(nal: ByteArray): Int {
        var index = 0
        while (index + 3 < nal.size) {
            if (nal[index] == 0.toByte() &&
                nal[index + 1] == 0.toByte() &&
                nal[index + 2] == 1.toByte()
            ) {
                return (nal[index + 3].toInt() and 0x1F)
            }
            index++
        }
        return -1
    }

    private fun nals(data: ByteArray): List<ByteArray> {
        val starts = mutableListOf<Int>()
        var index = 0
        while (index + 3 < data.size) {
            if (data[index] == 0.toByte() &&
                data[index + 1] == 0.toByte() &&
                data[index + 2] == 1.toByte()
            ) {
                starts += index
                index += 3
            } else {
                index++
            }
        }
        if (starts.isEmpty()) return emptyList()
        return starts.indices.mapNotNull { position ->
            val start = starts[position]
            val end = if (position + 1 < starts.size) starts[position + 1] else data.size
            if (end - start <= 3) null else data.copyOfRange(start, end)
        }
    }
}

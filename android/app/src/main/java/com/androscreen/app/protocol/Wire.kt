package com.androscreen.app.protocol

/**
 * One frame exchanged with the host daemon.
 *
 * The 12-byte header is `"ASCR"`, protocol version, kind, flags,
 * reserved, and a little-endian payload length.
 */
class Frame(val kind: Int, val flags: Int, val payload: ByteArray) {
    val isKeyframe: Boolean get() = flags and Wire.FLAG_KEYFRAME != 0

    /**
     * Whether a pong says the host holds no session with this phone.
     *
     * A daemon restarted with the cable still in answers pings but has forgotten
     * every greeting, so the phone has to send a fresh one. An empty payload
     * comes from a host that predates the flag, which means the session is live.
     */
    val awaitingHello: Boolean get() = payload.firstOrNull() == 1.toByte()
}

object Wire {
    const val PROTOCOL_VERSION = 1

    const val KIND_CONTROL = 1
    const val KIND_VIDEO = 2
    const val KIND_AUDIO = 3
    const val KIND_TOUCH = 4
    const val KIND_PING = 5
    const val KIND_PONG = 6
    const val KIND_CURSOR = 7

    const val FLAG_KEYFRAME = 1

    private const val HEADER_LEN = 12
    private val MAGIC = byteArrayOf(
        'A'.code.toByte(),
        'S'.code.toByte(),
        'C'.code.toByte(),
        'R'.code.toByte(),
    )

    fun encode(kind: Int, payload: ByteArray, flags: Int = 0): ByteArray {
        val out = ByteArray(HEADER_LEN + payload.size)
        MAGIC.copyInto(out, 0)
        out[4] = PROTOCOL_VERSION.toByte()
        out[5] = kind.toByte()
        out[6] = flags.toByte()
        out[8] = (payload.size and 0xFF).toByte()
        out[9] = ((payload.size ushr 8) and 0xFF).toByte()
        out[10] = ((payload.size ushr 16) and 0xFF).toByte()
        out[11] = ((payload.size ushr 24) and 0xFF).toByte()
        payload.copyInto(out, HEADER_LEN)
        return out
    }
}

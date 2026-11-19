package com.androscreen.app.protocol

import android.view.MotionEvent
import kotlin.math.roundToInt

/**
 * Converts Android [MotionEvent]s into the host's compact touch report.
 *
 * Coordinates are normalised to `0..=65535` so the phone does not need to
 * know the virtual display resolution.
 */
object TouchCodec {
    private const val MAX_POINTS = 10
    private const val POINT_SIZE = 7

    private const val ACTION_DOWN = 0
    private const val ACTION_MOVE = 1
    private const val ACTION_UP = 2
    private const val ACTION_CANCEL = 3

    fun encode(event: MotionEvent, width: Int, height: Int): ByteArray? {
        if (width <= 0 || height <= 0) return null
        val points = when (event.actionMasked) {
            MotionEvent.ACTION_DOWN,
            MotionEvent.ACTION_POINTER_DOWN,
            -> listOf(contact(event, event.actionIndex, ACTION_DOWN, width, height))

            MotionEvent.ACTION_MOVE -> buildList {
                for (index in 0 until event.pointerCount.coerceAtMost(MAX_POINTS)) {
                    add(contact(event, index, ACTION_MOVE, width, height))
                }
            }

            MotionEvent.ACTION_UP,
            MotionEvent.ACTION_POINTER_UP,
            -> listOf(contact(event, event.actionIndex, ACTION_UP, width, height))

            MotionEvent.ACTION_CANCEL -> listOf(
                Contact(id = 0, action = ACTION_CANCEL, x = 0, y = 0, pressure = 0),
            )

            else -> return null
        }
        if (points.isEmpty()) return null

        val out = ByteArray(1 + points.size * POINT_SIZE)
        out[0] = points.size.toByte()
        var at = 1
        for (point in points) {
            out[at++] = point.id.toByte()
            out[at++] = point.action.toByte()
            out[at++] = (point.x and 0xFF).toByte()
            out[at++] = ((point.x ushr 8) and 0xFF).toByte()
            out[at++] = (point.y and 0xFF).toByte()
            out[at++] = ((point.y ushr 8) and 0xFF).toByte()
            out[at++] = point.pressure.toByte()
        }
        return out
    }

    private fun contact(
        event: MotionEvent,
        index: Int,
        action: Int,
        width: Int,
        height: Int,
    ): Contact = Contact(
        id = event.getPointerId(index) and 0xFF,
        action = action,
        x = normalize(event.getX(index), width),
        y = normalize(event.getY(index), height),
        pressure = (event.getPressure(index).coerceIn(0f, 1f) * 255f).roundToInt(),
    )

    private fun normalize(value: Float, extent: Int): Int {
        if (extent <= 1) return 0
        val clamped = value.coerceIn(0f, (extent - 1).toFloat())
        return (clamped / (extent - 1) * 65_535f).roundToInt().coerceIn(0, 65_535)
    }

    private class Contact(
        val id: Int,
        val action: Int,
        val x: Int,
        val y: Int,
        val pressure: Int,
    )
}

package com.androscreen.app.ui

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.RectF
import android.view.View
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView

/**
 * The pointer the host sends separately from the picture.
 *
 * The compositor's cursor never enters the video stream, so its latency and
 * refresh rate belong to this view rather than to the decoder. Positions are
 * normalized, which lets the view place the bitmap without knowing the
 * desktop's resolution.
 */
class CursorState {
    @Volatile
    var isVisible = false
        private set

    /** Horizontal position across the picture, `0..1`. */
    @Volatile
    var x = 0f
        private set

    /** Vertical position across the picture, `0..1`. */
    @Volatile
    var y = 0f
        private set

    @Volatile
    var bitmap: Bitmap? = null
        private set

    /** Hotspot inside the bitmap, in bitmap pixels. */
    @Volatile
    var hotspotX = 0f
        private set

    @Volatile
    var hotspotY = 0f
        private set

    /** Width of the virtual display the bitmap is scaled against. */
    @Volatile
    var screenWidth = 0
        private set

    @Volatile
    var screenHeight = 0
        private set

    @Volatile
    private var view: CursorView? = null

    /** Whether a position has arrived, so a shape alone draws nothing. */
    @Volatile
    private var hasPosition = false

    /** Applies one cursor payload; called on the USB reader thread. */
    fun update(payload: ByteArray) {
        if (payload.size < 2) return
        val visibleNow = payload[1].toInt() and 1 != 0
        when (payload[0].toInt() and 0xFF) {
            MOVE -> {
                if (payload.size < 6) return
                x = readU16(payload, 2) / 65_535f
                y = readU16(payload, 4) / 65_535f
                hasPosition = true
                isVisible = visibleNow
                requestDraw()
            }
            SHAPE -> {
                if (payload.size < SHAPE_HEADER) return
                val width = readU16(payload, 2)
                val height = readU16(payload, 4)
                if (width <= 0 || height <= 0 || width > MAX_SHAPE || height > MAX_SHAPE) return
                val pixels = width * height
                if (payload.size < SHAPE_HEADER + pixels * 4) return
                val colors = IntArray(pixels)
                var at = SHAPE_HEADER
                for (index in colors.indices) {
                    val red = payload[at].toInt() and 0xFF
                    val green = payload[at + 1].toInt() and 0xFF
                    val blue = payload[at + 2].toInt() and 0xFF
                    val alpha = payload[at + 3].toInt() and 0xFF
                    colors[index] = (alpha shl 24) or (red shl 16) or (green shl 8) or blue
                    at += 4
                }
                bitmap = Bitmap.createBitmap(colors, width, height, Bitmap.Config.ARGB_8888)
                hotspotX = readI16(payload, 6).toFloat()
                hotspotY = readI16(payload, 8).toFloat()
                screenWidth = readU16(payload, 10)
                screenHeight = readU16(payload, 12)
                // The bitmap alone has no place to go yet; the move that
                // follows every shape puts it where the compositor has it.
                isVisible = visibleNow && hasPosition
                requestDraw()
            }
        }
    }

    /** Drops the pointer when the stream it belongs to goes away. */
    fun clear() {
        isVisible = false
        bitmap = null
        x = 0f
        y = 0f
        hasPosition = false
        requestDraw()
    }

    fun attach(target: CursorView) {
        view = target
    }

    fun detach() {
        view = null
    }

    private fun requestDraw() {
        view?.postInvalidateOnAnimation()
    }

    private fun readU16(source: ByteArray, at: Int): Int =
        (source[at].toInt() and 0xFF) or ((source[at + 1].toInt() and 0xFF) shl 8)

    private fun readI16(source: ByteArray, at: Int): Int =
        readU16(source, at).toShort().toInt()

    private companion object {
        const val MOVE = 0
        const val SHAPE = 1
        const val SHAPE_HEADER = 14

        /** A cursor bitmap larger than this is not one a desktop sends. */
        const val MAX_SHAPE = 512
    }
}

/** Draws the host's pointer over the decoded picture. */
class CursorView(context: Context) : View(context) {
    private val paint = Paint().apply {
        isFilterBitmap = true
        isAntiAlias = false
    }
    private val destination = RectF()
    private var state: CursorState? = null

    fun bind(target: CursorState) {
        state = target
    }

    override fun onDraw(canvas: Canvas) {
        val state = state ?: return
        val bitmap = state.bitmap ?: return
        if (!state.isVisible) return
        val screenWidth = state.screenWidth
        if (screenWidth <= 0 || width <= 0 || height <= 0) return
        val scale = width.toFloat() / screenWidth
        val left = state.x * width - state.hotspotX * scale
        val top = state.y * height - state.hotspotY * scale
        destination.set(
            left,
            top,
            left + bitmap.width * scale,
            top + bitmap.height * scale,
        )
        canvas.drawBitmap(bitmap, null, destination, paint)
    }
}

/** Places the pointer layer above the picture. */
@Composable
fun CursorOverlay(state: CursorState, modifier: Modifier = Modifier) {
    DisposableEffect(state) {
        onDispose { state.detach() }
    }
    AndroidView(
        modifier = modifier,
        factory = { context ->
            CursorView(context).apply {
                bind(state)
                state.attach(this)
            }
        },
    )
}

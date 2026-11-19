package com.androscreen.app.ui

import android.annotation.SuppressLint
import android.provider.Settings
import android.graphics.PixelFormat
import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.requiredWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import com.androscreen.app.protocol.TouchCodec
import kotlinx.coroutines.delay

/**
 * The secondary display: a SurfaceView the decoder renders into, with
 * multi-touch forwarded to the host.
 */
// The SurfaceView is a touch surface for the remote desktop, not a clickable
// control, so performClick() semantics do not apply.
@SuppressLint("ClickableViewAccessibility")
@Composable
fun DisplayScreen(
    videoStarted: Boolean,
    videoSeen: Boolean,
    videoWidth: Int,
    videoHeight: Int,
    cursor: CursorState,
    notice: String?,
    onDismissNotice: () -> Unit,
    onSurface: (android.view.Surface?) -> Unit,
    onTouch: (ByteArray) -> Unit,
    onExit: () -> Unit,
) {
    BackHandler(onBack = onExit)
    val context = LocalContext.current
    val surfaceCallback by rememberUpdatedState(onSurface)
    val touchCallback by rememberUpdatedState(onTouch)
    val dismissCallback by rememberUpdatedState(onDismissNotice)

    // A picture that has been on screen before and is missing now is a display
    // being rebuilt for a turn, which looks nothing like a screen that has not
    // started yet and must not be described the same way.
    val restarting = videoSeen && !videoStarted

    LaunchedEffect(notice) {
        if (notice != null) {
            delay(5000)
            dismissCallback()
        }
    }

    BoxWithConstraints(
        Modifier
            .fillMaxSize()
            .background(Color.Black)
            .then(if (restarting) Modifier.background(TransitionBackdrop) else Modifier),
        contentAlignment = Alignment.Center,
    ) {
        val pictureRatio = if (videoHeight > 0) {
            videoWidth.toFloat() / videoHeight.toFloat()
        } else {
            1f
        }
        val screenRatio = if (maxHeight.value > 0f) {
            maxWidth.value / maxHeight.value
        } else {
            1f
        }
        // The band the old picture leaves is either to its sides or above and
        // below it, and the note has to go there — anywhere else it lands under
        // the SurfaceView and is never seen.
        val noteOnTheSide = pictureRatio < screenRatio * 0.92f

        Box(
            modifier = if (videoWidth > 0 && videoHeight > 0) {
                Modifier.aspectRatio(
                    pictureRatio,
                    matchHeightConstraintsFirst = pictureRatio < 1f,
                )
            } else {
                Modifier.fillMaxSize()
            }.then(
                // Framed, the leftover shape reads as a window being resized
                // rather than as the picture having come out wrong.
                if (restarting) {
                    Modifier.border(
                        width = 1.dp,
                        color = Palette.Cyan.copy(alpha = 0.45f),
                        shape = RoundedCornerShape(3.dp),
                    )
                } else {
                    Modifier
                },
            ),
            contentAlignment = Alignment.Center,
        ) {
            AndroidView(
                // The computer sends the desktop as one picture, and a SurfaceView
                // fills whatever box it is given. Held the way the display has not
                // yet caught up with, that box is the whole panel, so the picture
                // is smeared across it; keeping the ratio it arrived at shows the
                // desktop whole and small instead, with the black saying the shape
                // is still on its way.
                modifier = Modifier.fillMaxSize(),
                factory = {
                    SurfaceView(context).apply {
                        keepScreenOn = true
                        // A SurfaceView below the app window is mirrored through it, and
                        // an idle window stops releasing consumed buffers, which starves
                        // the decoder after four frames. An overlay surface is consumed
                        // by the compositor directly.
                        setZOrderMediaOverlay(true)
                        holder.setFormat(PixelFormat.OPAQUE)
                        holder.addCallback(
                            object : SurfaceHolder.Callback {
                                override fun surfaceCreated(holder: SurfaceHolder) {
                                    surfaceCallback(holder.surface)
                                }

                                override fun surfaceChanged(
                                    holder: SurfaceHolder,
                                    format: Int,
                                    width: Int,
                                    height: Int,
                                ) = Unit

                                override fun surfaceDestroyed(holder: SurfaceHolder) {
                                    surfaceCallback(null)
                                }
                            },
                        )
                        setOnTouchListener { view, event ->
                            val payload = TouchCodec.encode(event, view.width, view.height)
                            if (payload != null) touchCallback(payload)
                            true
                        }
                    }
                },
            )
            // The host keeps the pointer out of the video, so it is drawn here
            // at the panel's own rate instead of the stream's.
            CursorOverlay(
                state = cursor,
                modifier = Modifier.fillMaxSize(),
            )
        }

        if (restarting) {
            SwitchingHint(
                modifier = if (noteOnTheSide) {
                    Modifier.align(Alignment.CenterEnd).padding(end = 24.dp)
                } else {
                    Modifier.align(Alignment.BottomCenter).padding(bottom = 32.dp)
                },
            )
        }

        if (!videoStarted && !videoSeen) {
            StartingOverlay(Modifier.align(Alignment.Center))
        }

        // Folding away is for a display that is already running: on a screen
        // that has not started yet the handle stays put, or a stuck start would
        // hide the only way out.
        ExitRail(
            running = videoSeen && videoStarted,
            onExit = onExit,
            modifier = Modifier.align(Alignment.TopEnd),
        )

        if (notice != null) {
            Box(
                Modifier
                    .align(Alignment.BottomCenter)
                    .windowInsetsPadding(
                        WindowInsets.safeDrawing.only(WindowInsetsSides.Bottom),
                    )
                    .padding(bottom = 24.dp, start = 24.dp, end = 24.dp),
            ) {
                Box(
                    Modifier
                        .widthIn(max = 520.dp)
                        .clip(RoundedCornerShape(16.dp))
                        .background(Color(0xCC101828))
                        .padding(horizontal = 18.dp, vertical = 12.dp),
                ) {
                    Text(
                        notice,
                        color = Color(0xFFE8ECF8),
                        fontSize = 13.sp,
                    )
                }
            }
        }
    }
}

/**
 * What the band around the old picture is painted as while the virtual display
 * is rebuilt for a turn.
 *
 * The decoder draws into a SurfaceView in the media overlay layer, which sits
 * above a background painted on the root box, so no mask drawn there can
 * cover the picture itself. Content composables placed after the AndroidView
 * do draw above the picture (confirmed by the Gate 0 probe, 2026-11-05).
 * The band around it, however, is ordinary window content and can be painted.
 * That band is what made a turn look broken: the picture is still 1344x3064
 * while the phone is already held at 2460x1080, so it collapses into a narrow
 * strip flanked by two wide slabs of pure black. Giving that black a shade, a
 * frame and a reason turns the same second into a transition.
 */
private val TransitionBackdrop = Brush.verticalGradient(
    listOf(Color(0xFF060A12), Color(0xFF121C2E), Color(0xFF060A12)),
)

/** Names what is happening while the picture is being rebuilt. */
@Composable
private fun SwitchingHint(modifier: Modifier = Modifier) {
    Column(modifier, horizontalAlignment = Alignment.CenterHorizontally) {
        CircularProgressIndicator(
            modifier = Modifier.size(26.dp),
            color = Palette.Cyan,
            strokeWidth = 2.5.dp,
        )
        Spacer(Modifier.height(10.dp))
        Text(
            "正在切换方向…",
            color = Palette.TextPrimary,
            fontSize = 13.sp,
            fontWeight = FontWeight.Medium,
        )
        Spacer(Modifier.height(3.dp))
        Text(
            "画面稍后自动恢复",
            color = Palette.TextSecondary,
            fontSize = 11.sp,
        )
    }
}

@Composable
private fun StartingOverlay(modifier: Modifier = Modifier) {    Box(
        modifier
            .clip(RoundedCornerShape(22.dp))
            .background(Color(0xD9101828))
            .padding(horizontal = 28.dp, vertical = 24.dp),
    ) {
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            CircularProgressIndicator(
                modifier = Modifier.size(34.dp),
                color = Palette.Cyan,
                strokeWidth = 3.dp,
            )
            Spacer(Modifier.height(14.dp))
            Text(
                "正在启动副屏…",
                color = Palette.TextPrimary,
                fontSize = 14.sp,
                fontWeight = FontWeight.Medium,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                "电脑端正在创建虚拟显示器",
                color = Palette.TextSecondary,
                fontSize = 12.sp,
            )
        }
    }
}

/**
 * The way out, parked at a fixed top-right corner position.
 *
 * The rail stays visible permanently: a control that fades to nothing cannot
 * be found again, especially after a turn. The end inset is deliberately NOT
 * applied - in landscape the camera cutout pushes safeDrawing.End in by about
 * 110px, moving the rail away from the edge people actually tap. A wide
 * invisible hit band absorbs that difference. A tap opens a compact panel,
 * and a 600ms hold on its only action leaves. The rail is kept deliberately
 * short while the hit band stays tall: shrinking the landmark must not shrink
 * the target. No text hint is shown - a permanently visible rail plus the
 * system back key are enough.
 */
@Composable
private fun ExitRail(
    running: Boolean,
    onExit: () -> Unit,
    modifier: Modifier = Modifier,
) {
    var expanded by remember { mutableStateOf(false) }
    var holding by remember { mutableStateOf(false) }
    var lastAction by remember { mutableStateOf(0) }

    val context = LocalContext.current
    val motionOff = remember {
        Settings.Global.getFloat(
            context.contentResolver,
            Settings.Global.ANIMATOR_DURATION_SCALE,
            1f,
        ) == 0f
    }

    // An open panel folds itself away. The rail itself never disappears: it is
    // the only landmark telling the user where the exit is.
    LaunchedEffect(expanded, running, lastAction) {
        if (!running) return@LaunchedEffect
        if (expanded) {
            delay(PANEL_COLLAPSE_MS)
            expanded = false
            lastAction++
        }
    }

    // The hold is the confirmation. Releasing early cancels the timer and the
    // ring together, so nothing is left one tap away from leaving.
    LaunchedEffect(holding) {
        if (holding) {
            delay(HOLD_MS)
            onExit()
        }
    }

    val railAlpha by animateFloatAsState(
        targetValue = if (expanded) 0f else RAIL_ALPHA,
        animationSpec = tween(if (motionOff) 0 else FADE_MS),
        label = "exit-rail-alpha",
    )
    val panelShift by animateFloatAsState(
        targetValue = if (expanded) 0f else 1f,
        animationSpec = tween(
            if (motionOff) 0 else if (expanded) GLIDE_IN_MS else GLIDE_OUT_MS,
        ),
        label = "exit-panel-shift",
    )
    val holdProgress by animateFloatAsState(
        targetValue = if (holding) 1f else 0f,
        animationSpec = tween(
            if (holding) HOLD_MS.toInt() else RELEASE_MS,
            easing = LinearEasing,
        ),
        label = "exit-hold-progress",
    )

    Box(
        modifier
            .fillMaxSize()
            .windowInsetsPadding(
                WindowInsets.safeDrawing.only(WindowInsetsSides.Top),
            )
            .semantics {
                contentDescription = if (expanded) {
                    "退出副屏，长按退出"
                } else {
                    "退出副屏，轻点展开"
                }
            },
    ) {
        // A wide band reaches both the physical edge and the area a system
        // gesture inset would otherwise steal. The rail sits a little inside
        // it so the visible landmark is never under a gesture zone.
        Box(
            Modifier
                .align(Alignment.TopEnd)
                .size(width = HIT_W_DP.dp, height = HIT_H_DP.dp)
                .then(
                    if (expanded) {
                        Modifier
                    } else {
                        Modifier.clickable(onClickLabel = "展开退出面板") {
                            expanded = true
                            lastAction++
                        }
                    },
                ),
            contentAlignment = Alignment.CenterEnd,
        ) {
            val railShape = RoundedCornerShape(RAIL_W_DP.dp / 2)
            Box(
                Modifier
                    .padding(end = RAIL_INSET_DP.dp)
                    .size(width = RAIL_W_DP.dp, height = RAIL_H_DP.dp)
                    .clip(railShape)
                    .background(Palette.Cyan.copy(alpha = railAlpha))
                    .border(0.5.dp, Color.Black.copy(alpha = 0.22f), railShape),
            )
        }

        // Removed from the tree once folded away, so a transparent panel can
        // never sit on top of the desktop and swallow a click.
        if (expanded || panelShift < 1f) {
            val panelShape = RoundedCornerShape(
                topStart = 22.dp,
                bottomStart = 22.dp,
            )
            Row(
                Modifier
                    .align(Alignment.TopEnd)
                    .padding(top = ((HIT_H_DP - PANEL_H_DP) / 2f).dp)
                    .requiredWidth(PANEL_W_DP.dp)
                    .height(PANEL_H_DP.dp)
                    .graphicsLayer {
                        translationX = PANEL_W_DP * density * panelShift
                        alpha = 1f - panelShift
                    }
                    .clip(panelShape)
                    .background(Color(0xE6101828))
                    .border(1.dp, Color.White.copy(alpha = 0.12f), panelShape),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(
                    Modifier
                        .size(40.dp)
                        .clickable(onClickLabel = "收起退出面板") {
                            expanded = false
                            holding = false
                            lastAction++
                        },
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        "\u203A",
                        color = Color.White.copy(alpha = 0.7f),
                        fontSize = 18.sp,
                    )
                }
                Row(
                    Modifier
                        .weight(1f)
                        .height(PANEL_H_DP.dp)
                        .pointerInput(Unit) {
                            awaitEachGesture {
                                awaitFirstDown(requireUnconsumed = false)
                                holding = true
                                waitForUpOrCancellation()
                                holding = false
                            }
                        }
                        .semantics { contentDescription = "长按退出副屏" }
                        .padding(end = 14.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    Canvas(Modifier.size(18.dp)) {
                        val stroke = Stroke(width = 2.dp.toPx())
                        // A track must be visible before the hold starts, or the
                        // ring gives no hint that anything is being filled.
                        drawArc(
                            color = Color.White.copy(alpha = 0.18f),
                            startAngle = 0f,
                            sweepAngle = 360f,
                            useCenter = false,
                            style = stroke,
                        )
                        drawArc(
                            color = Palette.Cyan,
                            startAngle = -90f,
                            sweepAngle = 360f * holdProgress,
                            useCenter = false,
                            style = stroke,
                        )
                    }
                    Text(
                        if (holding) "继续按住…" else "长按退出",
                        color = if (holding) Palette.Cyan else Palette.TextPrimary,
                        fontSize = 13.sp,
                        fontWeight = FontWeight.Medium,
                    )
                }
            }
        }

    }
}

private const val RAIL_W_DP = 5f
private const val RAIL_H_DP = 44f
private const val RAIL_INSET_DP = 8f
private const val HIT_W_DP = 48f
private const val HIT_H_DP = 120f
private const val PANEL_W_DP = 144f
private const val PANEL_H_DP = 44f
private const val RAIL_ALPHA = 0.45f
private const val PANEL_COLLAPSE_MS = 4000L
private const val HOLD_MS = 600L
private const val GLIDE_IN_MS = 180
private const val GLIDE_OUT_MS = 240
private const val FADE_MS = 320
private const val RELEASE_MS = 120

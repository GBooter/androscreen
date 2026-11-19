package com.androscreen.app.ui

import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.sp
import com.androscreen.app.R
import com.androscreen.app.protocol.HostStats
import com.androscreen.app.protocol.HostTemp
import java.util.Locale
import kotlin.math.cos
import kotlin.math.roundToInt
import kotlin.math.sin

/**
 * Idle screen: live CPU, memory, and temperature of the computer, plus the
 * entry point for using the phone as a secondary display.
 */
@Composable
fun DashboardScreen(
    stats: HostStats?,
    tempHistory: List<Float>,
    tempsSupported: Boolean,
    attached: Boolean,
    notice: String?,
    onStart: () -> Unit,
    onDismissNotice: () -> Unit,
    onSettings: () -> Unit,
    onRelease: () -> Unit,
) {
    Box(
        Modifier
            .fillMaxSize()
            .background(
                Brush.verticalGradient(
                    listOf(Palette.BackgroundTop, Palette.BackgroundBottom),
                ),
            ),
    ) {
        Glow()
        BoxWithConstraints(
            Modifier
                .fillMaxSize()
                .padding(WindowInsets.safeDrawing.asPaddingValues()),
        ) {
            // The same information blocks, arranged for the space available:
            // one column when tall, three when wide. Neither layout drops data.
            if (maxWidth > maxHeight) {
                WideDashboard(
                    stats,
                    tempHistory,
                    tempsSupported,
                    attached,
                    notice,
                    onStart,
                    onDismissNotice,
                    onSettings,
                    onRelease,
                )
            } else {
                TallDashboard(
                    stats,
                    tempHistory,
                    tempsSupported,
                    attached,
                    notice,
                    onStart,
                    onDismissNotice,
                    onSettings,
                    onRelease,
                )
            }
        }
    }
}

@Composable
private fun TallDashboard(
    stats: HostStats?,
    tempHistory: List<Float>,
    tempsSupported: Boolean,
    attached: Boolean,
    notice: String?,
    onStart: () -> Unit,
    onDismissNotice: () -> Unit,
    onSettings: () -> Unit,
    onRelease: () -> Unit,
) {
    // Everything fits on one screen: the repeated settings row is gone, the
    // dials are smaller, and the footer holds only the action and its note.
    // Scrolling stays as a safety net for a notice, not as the normal path.
    Column(
        Modifier
            .fillMaxSize()
            .padding(horizontal = 18.dp, vertical = 12.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Header(attached, onSettings)
        Column(
            Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Gauges(stats, gaugeSize = 104.dp)
            TemperatureCard(stats, tempHistory, tempsSupported, sparkHeight = 28.dp)
            CoreLoadCard(stats, barsHeight = 40.dp)
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                UptimeCard(stats, Modifier.weight(1f))
                LoadCard(stats, Modifier.weight(1f))
            }
            if (notice != null) {
                NoticeCard(notice, onDismissNotice)
            }
        }
        StartButton(attached, onStart)
        Row(verticalAlignment = Alignment.CenterVertically) {
            Hint(attached, Modifier.weight(1f))
            if (attached) {
                Spacer(Modifier.width(8.dp))
                FileTransferLink(onRelease)
            }
        }
    }
}

@Composable
private fun WideDashboard(
    stats: HostStats?,
    tempHistory: List<Float>,
    tempsSupported: Boolean,
    attached: Boolean,
    notice: String?,
    onStart: () -> Unit,
    onDismissNotice: () -> Unit,
    onSettings: () -> Unit,
    onRelease: () -> Unit,
) {
    // Three columns use the 820dp width instead of leaving a third of the
    // screen empty. Wide-shaped data (per-core bars, temperature trend) belongs
    // here more than it ever belonged in the tall column.
    Column(
        Modifier
            .fillMaxSize()
            .padding(horizontal = 20.dp, vertical = 10.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Header(attached, onSettings)
        if (notice != null) {
            NoticeCard(notice, onDismissNotice)
        }
        Row(
            Modifier.weight(1f),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Column(
                Modifier.weight(1.2f),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Gauges(stats, gaugeSize = 84.dp)
                TemperatureCard(
                    stats,
                    tempHistory,
                    tempsSupported,
                    compact = true,
                    sparkHeight = 20.dp,
                )
            }
            Column(
                Modifier.weight(1.35f),
                verticalArrangement = Arrangement.SpaceBetween,
            ) {
                CoreLoadCard(stats, barsHeight = 64.dp, compact = true)
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    UptimeCard(stats, Modifier.weight(1f))
                    LoadCard(stats, Modifier.weight(1f))
                }
                Hint(attached)
            }
            Column(
                Modifier
                    .weight(1f)
                    .clip(RoundedCornerShape(20.dp))
                    .background(
                        Brush.verticalGradient(
                            listOf(
                                Palette.Cyan.copy(alpha = 0.10f),
                                Palette.Violet.copy(alpha = 0.06f),
                            ),
                        ),
                    )
                    .padding(12.dp),
                verticalArrangement = Arrangement.SpaceBetween,
            ) {
                Column {
                    Text(
                        if (attached) "已就绪" else "等待连接",
                        color = if (attached) Palette.Green else Palette.Amber,
                        fontSize = 14.sp,
                        fontWeight = FontWeight.Medium,
                    )
                    Spacer(Modifier.height(4.dp))
                    Text(
                        if (attached) {
                            "手机可用作电脑的扩展屏幕"
                        } else {
                            "插入数据线后自动连接"
                        },
                        color = Palette.TextSecondary,
                        fontSize = 11.sp,
                    )
                }
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    StartButton(attached, onStart)
                    if (attached) {
                        FileTransferLink(onRelease)
                    }
                }
            }
        }
    }
}

@Composable
private fun Gauges(stats: HostStats?, gaugeSize: Dp = 112.dp) {
    Row(horizontalArrangement = Arrangement.spacedBy(14.dp)) {
        GaugeCard(
            title = "处理器",
            value = stats?.cpuPercent,
            color = Palette.Cyan,
            detail = stats?.let { "${it.perCorePercent.size.coerceAtLeast(1)} 线程" }
                ?: "等待电脑数据",
            gaugeSize = gaugeSize,
            modifier = Modifier.weight(1f),
        )
        GaugeCard(
            title = "内存",
            value = stats?.memUsedPercent,
            color = Palette.Violet,
            detail = stats?.let { "${gib(it.memUsedKib)} / ${gib(it.memTotalKib)}" }
                ?: "等待电脑数据",
            gaugeSize = gaugeSize,
            modifier = Modifier.weight(1f),
        )
    }
}

@Composable
private fun CoreLoadCard(
    stats: HostStats?,
    barsHeight: Dp = 58.dp,
    compact: Boolean = false,
    modifier: Modifier = Modifier,
) {
    SectionCard(modifier = modifier, compact = compact) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                "每核负载",
                color = Palette.TextSecondary,
                fontSize = 13.sp,
            )
            Spacer(Modifier.weight(1f))
            Text(
                stats?.perCorePercent?.size?.let { "$it 线程" } ?: "—",
                color = Palette.TextSecondary,
                fontSize = 12.sp,
            )
        }
        Spacer(Modifier.height(if (compact) 8.dp else 12.dp))
        CoreBars(stats?.perCorePercent.orEmpty(), barsHeight)
    }
}

/**
 * What the computer is running at, temperature-wise.
 *
 * This is the number a phone is genuinely useful for: a laptop with the lid
 * shut under a desk has no readable display, so the idle phone becomes the
 * thermometer. The line underneath is the last half minute, because a single
 * reading cannot tell a warm idle from a cooling fan spin-up.
 */
@Composable
private fun TemperatureCard(
    stats: HostStats?,
    history: List<Float>,
    tempsSupported: Boolean,
    modifier: Modifier = Modifier,
    compact: Boolean = false,
    sparkHeight: Dp = 34.dp,
) {
    val temps = stats?.temps.orEmpty()
    val hottest = stats?.hottest
    val gap = if (compact) 4.dp else 12.dp
    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(20.dp))
            .background(Palette.Card)
            .padding(
                horizontal = if (compact) 12.dp else 16.dp,
                vertical = if (compact) 6.dp else 14.dp,
            ),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("电脑温度", color = Palette.TextSecondary, fontSize = 13.sp)
            Spacer(Modifier.weight(1f))
            Text(
                when {
                    hottest != null -> "最高 ${temperature(hottest.celsius)}°C"
                    stats == null -> "等待电脑数据"
                    tempsSupported -> "无传感器"
                    else -> "待更新"
                },
                color = hottest?.let { heatColor(it.celsius) } ?: Palette.TextSecondary,
                fontSize = 12.sp,
                fontWeight = FontWeight.Medium,
            )
        }
        Spacer(Modifier.height(gap))
        if (temps.isEmpty()) {
            Text(
                when {
                    stats == null -> "插好数据线后，这里会显示电脑各部件的实时温度"
                    !tempsSupported -> "电脑端程序需要更新到 0.4.0 才能上报温度"
                    else -> "这台电脑没有可读取的温度传感器"
                },
                color = Palette.TextSecondary,
                fontSize = 11.sp,
            )
        } else if (compact) {
            // A landscape column is short and wide: the numbers belong on one
            // line, with the trend underneath, rather than in two tall chips.
            Row(verticalAlignment = Alignment.CenterVertically) {
                temps.take(3).forEach { temp ->
                    Text(
                        "${temp.label} ${temperature(temp.celsius)}°C",
                        color = heatColor(temp.celsius),
                        fontSize = 13.sp,
                        fontWeight = FontWeight.Medium,
                        fontFamily = FontFamily.Monospace,
                        modifier = Modifier.padding(end = 10.dp),
                    )
                }
            }
        } else {
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                temps.forEach { temp -> TempChip(temp, Modifier.weight(1f)) }
            }
        }
        if (history.size >= 2) {
            Spacer(Modifier.height(gap))
            TempSparkline(history, sparkHeight)
        }
    }
}

@Composable
private fun RowScope.TempChip(temp: HostTemp, modifier: Modifier = Modifier) {
    val color = heatColor(temp.celsius)
    Column(
        modifier = modifier
            .clip(RoundedCornerShape(14.dp))
            .background(color.copy(alpha = 0.10f))
            .padding(horizontal = 10.dp, vertical = 8.dp),
    ) {
        Text(
            temp.label,
            color = Palette.TextSecondary,
            fontSize = 11.sp,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Spacer(Modifier.height(2.dp))
        Row(verticalAlignment = Alignment.Bottom) {
            Text(
                temperature(temp.celsius),
                color = color,
                fontSize = 18.sp,
                fontWeight = FontWeight.SemiBold,
                fontFamily = FontFamily.Monospace,
            )
            Spacer(Modifier.width(2.dp))
            Text("°C", color = color.copy(alpha = 0.75f), fontSize = 10.sp)
        }
    }
}

/** The warmest sensor over the last samples, as a soft filled line. */
@Composable
private fun TempSparkline(history: List<Float>, height: Dp = 34.dp) {
    val color = heatColor(history.last())
    Canvas(
        Modifier
            .fillMaxWidth()
            .height(height),
    ) {
        if (history.size < 2) return@Canvas
        var peak = history[0]
        var floor = history[0]
        for (value in history) {
            if (value > peak) peak = value
            if (value < floor) floor = value
        }
        // A flat line still deserves visible shape, so give every series at
        // least a few degrees of range instead of a single straight edge.
        val span = (peak - floor).coerceAtLeast(8f)
        val step = size.width / (history.size - 1).toFloat()
        val line = Path()
        history.forEachIndexed { index, value ->
            val x = index * step
            val y = size.height * (1f - (value - floor) / span)
            if (index == 0) line.moveTo(x, y) else line.lineTo(x, y)
        }
        val area = Path().apply {
            addPath(line)
            lineTo(size.width, size.height)
            lineTo(0f, size.height)
            close()
        }
        drawPath(
            area,
            brush = Brush.verticalGradient(
                listOf(color.copy(alpha = 0.26f), Color.Transparent),
            ),
        )
        drawPath(line, color = color, style = Stroke(width = 2.dp.toPx(), cap = StrokeCap.Round))
    }
}

/** Green below a warm idle, amber where a fan is audible, red where it matters. */
private fun heatColor(celsius: Float): Color = when {
    celsius >= 85f -> Palette.Danger
    celsius >= 70f -> Palette.Amber
    else -> Palette.Green
}

private fun temperature(celsius: Float): String = "${celsius.roundToInt()}"

@Composable
private fun UptimeCard(stats: HostStats?, modifier: Modifier = Modifier) {
    InfoCard(
        title = "运行时长",
        value = stats?.let { uptime(it.uptimeSecs) } ?: "—",
        hint = "电脑已持续运行",
        modifier = modifier,
    )
}
@Composable
private fun LoadCard(stats: HostStats?, modifier: Modifier = Modifier) {
    InfoCard(
        title = "系统负载",
        // No spaces around the slashes: the widest of these cards is still
        // narrow enough to cut off a three digit load if it is padded out.
        value = stats?.let {
            "${load(it.loadOne)}/${load(it.loadFive)}/${load(it.loadFifteen)}"
        } ?: "—",
        hint = "1 / 5 / 15 分钟",
        modifier = modifier,
        valueSize = 13.sp,
    )
}

@Composable
private fun Hint(attached: Boolean, modifier: Modifier = Modifier) {
    Text(
        if (attached) {
            "保持亮屏 · 支持多指触控"
        } else {
            "插线自动连接 · 无需 USB 调试"
        },
        color = Palette.TextSecondary,
        fontSize = 11.sp,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = modifier,
    )
}

@Composable
private fun Glow() {
    Canvas(Modifier.fillMaxSize()) {
        drawCircle(
            brush = Brush.radialGradient(
                colors = listOf(Palette.Cyan.copy(alpha = 0.10f), Color.Transparent),
                center = Offset(size.width * 0.15f, size.height * 0.12f),
                radius = size.width * 0.6f,
            ),
            radius = size.width * 0.6f,
            center = Offset(size.width * 0.15f, size.height * 0.12f),
        )
        drawCircle(
            brush = Brush.radialGradient(
                colors = listOf(Palette.Violet.copy(alpha = 0.10f), Color.Transparent),
                center = Offset(size.width * 0.9f, size.height * 0.55f),
                radius = size.width * 0.55f,
            ),
            radius = size.width * 0.55f,
            center = Offset(size.width * 0.9f, size.height * 0.55f),
        )
    }
}

@Composable
private fun Header(attached: Boolean, onSettings: () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(
            Modifier
                .size(42.dp)
                .clip(RoundedCornerShape(14.dp))
                .background(Brush.linearGradient(listOf(Palette.Cyan, Palette.Violet))),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                "A",
                color = Color(0xFF06121C),
                fontSize = 22.sp,
                fontWeight = FontWeight.Black,
            )
        }
        Spacer(Modifier.width(12.dp))
        Column {
            Text(
                // Chinese builds read 安卓副屏, everything else AndroScreen.
                stringResource(R.string.app_name),
                color = Palette.TextPrimary,
                fontSize = 19.sp,
                fontWeight = FontWeight.SemiBold,
                letterSpacing = 0.4.sp,
            )
            Text(
                "USB 扩展副屏",
                color = Palette.TextSecondary,
                fontSize = 12.sp,
            )
        }
        Spacer(Modifier.weight(1f))
        StatusPill(attached)
        Spacer(Modifier.width(10.dp))
        SettingsButton(onSettings)
    }
}

/**
 * The way into the settings screen.
 *
 * It sits beside the status pill because that is where a user looks when the
 * service does something they did not ask for, and turning it off has to be
 * one tap away.
 */
@Composable
private fun SettingsButton(onSettings: () -> Unit) {
    Box(
        Modifier
            .size(44.dp)
            .clip(CircleShape)
            .background(Color(0x14FFFFFF))
            .clickable { onSettings() },
        contentAlignment = Alignment.Center,
    ) {
        Text("\u2699", color = Palette.TextSecondary, fontSize = 17.sp)
    }
}

@Composable
private fun StatusPill(attached: Boolean) {
    val transition = rememberInfiniteTransition(label = "status")
    val pulse by transition.animateFloat(
        initialValue = 0.35f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 900),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "status-pulse",
    )
    val color = if (attached) Palette.Green else Palette.Amber
    Row(
        Modifier
            .clip(RoundedCornerShape(50))
            .background(Color(0x14FFFFFF))
            .padding(horizontal = 12.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .size(8.dp)
                .clip(CircleShape)
                .background(color.copy(alpha = if (attached) pulse else 0.8f)),
        )
        Spacer(Modifier.width(8.dp))
        Text(
            if (attached) "已连接" else "等待连接",
            color = color,
            fontSize = 12.sp,
            fontWeight = FontWeight.Medium,
        )
    }
}

@Composable
private fun GaugeCard(
    title: String,
    value: Float?,
    color: Color,
    detail: String,
    modifier: Modifier = Modifier,
    gaugeSize: Dp = 132.dp,
) {
    val target = if (value == null) 0f else (value / 100f).coerceIn(0f, 1f)
    val progress by animateFloatAsState(
        targetValue = target,
        animationSpec = tween(durationMillis = 600),
        label = "$title-gauge",
    )
    // A small gauge only appears on a short screen, where the card has to
    // spend its height on the dial rather than on its own margins.
    val margin = if (gaugeSize >= 108.dp) 14.dp else 10.dp
    val gap = if (gaugeSize >= 108.dp) 10.dp else 6.dp
    Column(
        modifier = modifier
            .clip(RoundedCornerShape(24.dp))
            .background(Palette.Card)
            .padding(vertical = margin, horizontal = 12.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(Modifier.size(gaugeSize), contentAlignment = Alignment.Center) {
            Canvas(Modifier.fillMaxSize()) {
                drawGauge(progress, color)
            }
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Text(
                    if (value == null) "—" else "${value.roundToInt()}%",
                    color = Palette.TextPrimary,
                    fontSize = if (gaugeSize >= 108.dp) 27.sp else 22.sp,
                    fontWeight = FontWeight.Bold,
                    fontFamily = FontFamily.Monospace,
                )
                Text(
                    title,
                    color = Palette.TextSecondary,
                    fontSize = 12.sp,
                )
            }
        }
        Spacer(Modifier.height(gap))
        Text(
            detail,
            color = Palette.TextSecondary,
            fontSize = 12.sp,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

private fun DrawScope.drawGauge(progress: Float, color: Color) {
    // Everything scales with the dial so the same code serves the tall and
    // the wide layout.
    val unit = size.minDimension
    val stroke = unit * 0.076f
    val inset = stroke * 1.55f
    val rect = androidx.compose.ui.geometry.Rect(inset, inset, size.width - inset, size.height - inset)
    val topLeft = Offset(rect.left, rect.top)
    val arcSize = Size(rect.width, rect.height)
    val center = Offset(size.width / 2f, size.height / 2f)
    val radius = rect.width / 2f

    drawArc(
        color = Palette.TextPrimary.copy(alpha = 0.06f),
        startAngle = 135f,
        sweepAngle = 270f,
        useCenter = false,
        topLeft = topLeft,
        size = arcSize,
        style = Stroke(width = stroke, cap = StrokeCap.Round),
    )

    for (index in 0..10) {
        val angle = Math.toRadians(135.0 + 27.0 * index)
        val lit = index / 10f <= progress
        val inner = radius - stroke - unit * 0.045f
        val outer = radius - stroke - unit * 0.008f
        drawLine(
            color = Palette.TextPrimary.copy(alpha = if (lit) 0.35f else 0.10f),
            start = Offset(
                center.x + cos(angle).toFloat() * inner,
                center.y + sin(angle).toFloat() * inner,
            ),
            end = Offset(
                center.x + cos(angle).toFloat() * outer,
                center.y + sin(angle).toFloat() * outer,
            ),
            strokeWidth = unit * 0.011f,
            cap = StrokeCap.Round,
        )
    }

    if (progress <= 0f) return

    drawArc(
        color = color.copy(alpha = 0.16f),
        startAngle = 135f,
        sweepAngle = 270f * progress,
        useCenter = false,
        topLeft = topLeft,
        size = arcSize,
        style = Stroke(width = stroke * 2.1f, cap = StrokeCap.Round),
    )
    drawArc(
        color = color,
        startAngle = 135f,
        sweepAngle = 270f * progress,
        useCenter = false,
        topLeft = topLeft,
        size = arcSize,
        style = Stroke(width = stroke, cap = StrokeCap.Round),
    )
    val end = Math.toRadians(135.0 + 270.0 * progress)
    val point = Offset(
        center.x + cos(end).toFloat() * radius,
        center.y + sin(end).toFloat() * radius,
    )
    drawCircle(color = color.copy(alpha = 0.25f), radius = stroke * 1.4f, center = point)
    drawCircle(color = color, radius = stroke * 0.62f, center = point)
}

@Composable
private fun CoreBars(cores: List<Float>, height: Dp = 58.dp) {
    Canvas(
        Modifier
            .fillMaxWidth()
            .height(height),
    ) {
        if (cores.isEmpty()) {
            drawRoundRect(
                color = Palette.TextPrimary.copy(alpha = 0.05f),
                size = Size(size.width, size.height),
                cornerRadius = CornerRadius(6.dp.toPx()),
            )
            return@Canvas
        }
        val gap = 2.5.dp.toPx()
        val barWidth = ((size.width - gap * (cores.size - 1)) / cores.size)
            .coerceAtLeast(1f)
        val corner = 3.dp.toPx()
        cores.forEachIndexed { index, value ->
            val x = index * (barWidth + gap)
            drawRoundRect(
                color = Palette.TextPrimary.copy(alpha = 0.05f),
                topLeft = Offset(x, 0f),
                size = Size(barWidth, size.height),
                cornerRadius = CornerRadius(corner),
            )
            val ratio = (value / 100f).coerceIn(0f, 1f)
            val barHeight = (size.height * ratio).coerceAtLeast(3.dp.toPx())
            drawRoundRect(
                brush = Brush.verticalGradient(
                    colors = listOf(Palette.Cyan, Palette.Violet.copy(alpha = 0.65f)),
                    startY = size.height - barHeight,
                    endY = size.height,
                ),
                topLeft = Offset(x, size.height - barHeight),
                size = Size(barWidth, barHeight),
                cornerRadius = CornerRadius(corner),
            )
        }
    }
}

@Composable
private fun InfoCard(
    title: String,
    value: String,
    hint: String,
    modifier: Modifier = Modifier,
    valueSize: TextUnit = 16.sp,
) {
    Column(
        modifier = modifier
            .clip(RoundedCornerShape(20.dp))
            .background(Palette.Card)
            .padding(horizontal = 14.dp, vertical = 12.dp),
    ) {
        Text(title, color = Palette.TextSecondary, fontSize = 12.sp)
        Spacer(Modifier.height(4.dp))
        Text(
            value,
            color = Palette.TextPrimary,
            fontSize = valueSize,
            fontWeight = FontWeight.SemiBold,
            fontFamily = FontFamily.Monospace,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Spacer(Modifier.height(4.dp))
        Text(hint, color = Palette.TextSecondary, fontSize = 11.sp, maxLines = 1)
    }
}

@Composable
private fun SectionCard(
    modifier: Modifier = Modifier,
    compact: Boolean = false,
    content: @Composable ColumnScope.() -> Unit,
) {
    Column(
        modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(20.dp))
            .background(Palette.Card)
            .padding(
                horizontal = if (compact) 12.dp else 16.dp,
                vertical = if (compact) 10.dp else 14.dp,
            ),
        content = content,
    )
}

@Composable
private fun NoticeCard(message: String, onDismiss: () -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(18.dp))
            .background(Palette.Amber.copy(alpha = 0.12f))
            .padding(start = 16.dp, end = 8.dp, top = 12.dp, bottom = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            message,
            color = Palette.Amber,
            fontSize = 13.sp,
            modifier = Modifier.weight(1f),
        )
        Text(
            "知道了",
            color = Palette.TextPrimary,
            fontSize = 13.sp,
            fontWeight = FontWeight.Medium,
            modifier = Modifier
                .clip(RoundedCornerShape(12.dp))
                .clickable(onClick = onDismiss)
                .padding(horizontal = 10.dp, vertical = 6.dp),
        )
    }
}

@Composable
private fun StartButton(enabled: Boolean, onClick: () -> Unit) {
    Box(
        Modifier
            .fillMaxWidth()
            .height(56.dp)
            .clip(RoundedCornerShape(20.dp))
            .background(
                if (enabled) {
                    Brush.horizontalGradient(listOf(Palette.Cyan, Palette.Violet))
                } else {
                    Brush.horizontalGradient(listOf(Color(0xFF232B44), Color(0xFF232B44)))
                },
            )
            .clickable(enabled = enabled, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            if (enabled) "用作扩展屏" else "等待数据线连接",
            color = if (enabled) Color(0xFF06121C) else Palette.TextSecondary,
            fontSize = 16.sp,
            fontWeight = FontWeight.SemiBold,
        )
    }
}

/**
 * Hands the cable back for the file manager.
 *
 * Accessory mode hides the phone's storage, and the cable is the only way to
 * both; a user who came for files needs this one tap, not a settings hunt.
 */
@Composable
private fun FileTransferLink(onRelease: () -> Unit, modifier: Modifier = Modifier) {
    Text(
        "改用文件传输（释放 USB）",
        color = Palette.Cyan,
        fontSize = 11.sp,
        maxLines = 1,
        modifier = modifier
            .clickable { onRelease() },
    )
}

private fun gib(kib: Long): String {
    val gigabytes = kib / 1024.0 / 1024.0
    return if (gigabytes >= 1.0) {
        String.format(Locale.US, "%.1f GB", gigabytes)
    } else {
        String.format(Locale.US, "%.0f MB", kib / 1024.0)
    }
}

private fun uptime(seconds: Long): String {
    val days = seconds / 86_400
    val hours = seconds % 86_400 / 3_600
    val minutes = seconds % 3_600 / 60
    return when {
        days > 0 -> "${days}天 ${hours}时"
        hours > 0 -> "${hours}时 ${minutes}分"
        else -> "${minutes}分"
    }
}

// One decimal: the card is narrow, and the third of the three numbers is the
// one that would be cut off first.
private fun load(value: Float): String = String.format(Locale.US, "%.1f", value)

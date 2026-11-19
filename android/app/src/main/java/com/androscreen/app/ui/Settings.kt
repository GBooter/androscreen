package com.androscreen.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
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
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.androscreen.app.R

/**
 * The switches a user needs: whether the phone takes over the screen by itself,
 * whether it plays the computer's sound, and how to hand the cable back to the
 * file manager without unplugging it.
 */
@Composable
fun SettingsScreen(
    autoStart: Boolean,
    hostAudio: Boolean,
    compatMode: Boolean,
    attached: Boolean,
    version: String,
    onAutoStart: (Boolean) -> Unit,
    onHostAudio: (Boolean) -> Unit,
    onCompatMode: (Boolean) -> Unit,
    onRelease: () -> Unit,
    onBack: () -> Unit,
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
        Column(
            Modifier
                .fillMaxSize()
                .padding(WindowInsets.safeDrawing.asPaddingValues())
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp, vertical = 14.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier
                        .size(38.dp)
                        .clip(CircleShape)
                        .background(Color(0x14FFFFFF))
                        .clickable { onBack() },
                    contentAlignment = Alignment.Center,
                ) {
                    Text("‹", color = Palette.TextPrimary, fontSize = 22.sp)
                }
                Spacer(Modifier.width(12.dp))
                Text(
                    "设置",
                    color = Palette.TextPrimary,
                    fontSize = 20.sp,
                    fontWeight = FontWeight.SemiBold,
                )
            }
            Spacer(Modifier.height(22.dp))

            SwitchCard(
                title = "插线后自动开始副屏",
                summary = if (autoStart) {
                    "插上数据线就自动把手机变成电脑副屏"
                } else {
                    "插线后只显示状态页，需要时再手动开始"
                },
                checked = autoStart,
                onCheckedChange = onAutoStart,
            )

            Spacer(Modifier.height(14.dp))

            SwitchCard(
                title = "播放电脑的声音",
                summary = if (hostAudio) {
                    "副屏运行时，电脑的声音从手机放出来"
                } else {
                    "只传画面，电脑的声音仍留在电脑上"
                },
                checked = hostAudio,
                onCheckedChange = onHostAudio,
            )

            Spacer(Modifier.height(14.dp))

            SwitchCard(
                title = "兼容模式（1080p / 30 帧）",
                summary = if (compatMode) {
                    "画面限制在 1080p、30 帧，下次开始副屏时生效"
                } else {
                    "按手机屏幕的分辨率传输，画面更清晰"
                },
                checked = compatMode,
                onCheckedChange = onCompatMode,
            )

            Spacer(Modifier.height(14.dp))

            SettingsCard {
                Column {
                    Text(
                        "改用文件传输",
                        color = Palette.TextPrimary,
                        fontSize = 15.sp,
                        fontWeight = FontWeight.Medium,
                    )
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "副屏和文件管理不能同时占着数据线。释放后手机会断开副屏，" +
                            "在电脑上就能看到手机存储。",
                        color = Palette.TextSecondary,
                        fontSize = 12.sp,
                    )
                    Spacer(Modifier.height(14.dp))
                    ActionButton(
                        text = if (attached) "释放 USB，改用文件传输" else "数据线未连接",
                        enabled = attached,
                        onClick = onRelease,
                    )
                }
            }

            Spacer(Modifier.height(18.dp))
            Text(
                "${stringResource(R.string.app_name)} $version",
                color = Palette.TextSecondary,
                fontSize = 12.sp,
                modifier = Modifier.padding(start = 4.dp),
            )
            Spacer(Modifier.height(24.dp))
        }
    }
}

@Composable
private fun SwitchCard(
    title: String,
    summary: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
) {
    SettingsCard {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    title,
                    color = Palette.TextPrimary,
                    fontSize = 15.sp,
                    fontWeight = FontWeight.Medium,
                )
                Spacer(Modifier.height(4.dp))
                Text(summary, color = Palette.TextSecondary, fontSize = 12.sp)
            }
            Spacer(Modifier.width(12.dp))
            Switch(
                checked = checked,
                onCheckedChange = onCheckedChange,
                colors = SwitchDefaults.colors(
                    checkedThumbColor = Color(0xFF06121C),
                    checkedTrackColor = Palette.Cyan,
                    uncheckedThumbColor = Palette.TextSecondary,
                    uncheckedTrackColor = Color(0x22FFFFFF),
                ),
            )
        }
    }
}

@Composable
private fun SettingsCard(content: @Composable () -> Unit) {
    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(18.dp))
            .background(Palette.Card)
            .padding(16.dp),
    ) {
        content()
    }
}

@Composable
private fun ActionButton(text: String, enabled: Boolean, onClick: () -> Unit) {
    val shape = RoundedCornerShape(14.dp)
    val background = if (enabled) {
        Brush.horizontalGradient(listOf(Palette.Cyan, Palette.Violet))
    } else {
        Brush.horizontalGradient(listOf(Color(0x14FFFFFF), Color(0x14FFFFFF)))
    }
    val label = if (enabled) Color(0xFF06121C) else Palette.TextSecondary
    Box(
        Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(background)
            .clickable(enabled = enabled) { onClick() }
            .padding(vertical = 13.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(text, color = label, fontSize = 14.sp, fontWeight = FontWeight.SemiBold)
    }
}

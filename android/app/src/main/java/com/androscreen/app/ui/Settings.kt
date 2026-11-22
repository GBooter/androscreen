package com.androscreen.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
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
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.androscreen.app.AppLanguage
import com.androscreen.app.R

/**
 * The switches a user needs: whether the phone takes over the screen by itself,
 * whether it plays the computer's sound, how to hand the cable back to the file
 * manager without unplugging it, and which language the app speaks.
 */
@Composable
fun SettingsScreen(
    autoStart: Boolean,
    hostAudio: Boolean,
    compatMode: Boolean,
    attached: Boolean,
    language: AppLanguage,
    version: String,
    onAutoStart: (Boolean) -> Unit,
    onHostAudio: (Boolean) -> Unit,
    onCompatMode: (Boolean) -> Unit,
    onLanguage: (AppLanguage) -> Unit,
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
                    stringResource(R.string.settings_title),
                    color = Palette.TextPrimary,
                    fontSize = 20.sp,
                    fontWeight = FontWeight.SemiBold,
                )
            }
            Spacer(Modifier.height(22.dp))

            SwitchCard(
                title = stringResource(R.string.auto_start_title),
                summary = stringResource(
                    if (autoStart) R.string.auto_start_on else R.string.auto_start_off,
                ),
                checked = autoStart,
                onCheckedChange = onAutoStart,
            )

            Spacer(Modifier.height(14.dp))

            SwitchCard(
                title = stringResource(R.string.host_audio_title),
                summary = stringResource(
                    if (hostAudio) R.string.host_audio_on else R.string.host_audio_off,
                ),
                checked = hostAudio,
                onCheckedChange = onHostAudio,
            )

            Spacer(Modifier.height(14.dp))

            SwitchCard(
                title = stringResource(R.string.compat_title),
                summary = stringResource(
                    if (compatMode) R.string.compat_on else R.string.compat_off,
                ),
                checked = compatMode,
                onCheckedChange = onCompatMode,
            )

            Spacer(Modifier.height(14.dp))

            LanguageCard(language, onLanguage)

            Spacer(Modifier.height(14.dp))

            SettingsCard {
                Column {
                    Text(
                        stringResource(R.string.file_transfer_title),
                        color = Palette.TextPrimary,
                        fontSize = 15.sp,
                        fontWeight = FontWeight.Medium,
                    )
                    Spacer(Modifier.height(4.dp))
                    Text(
                        stringResource(R.string.file_transfer_body),
                        color = Palette.TextSecondary,
                        fontSize = 12.sp,
                    )
                    Spacer(Modifier.height(14.dp))
                    ActionButton(
                        text = stringResource(
                            if (attached) R.string.release_usb else R.string.no_cable,
                        ),
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

/**
 * The language choice, as three chips rather than a list.
 *
 * Three options fit on one line, and the two named languages are written in
 * themselves — someone who cannot read the current language still recognises
 * "中文" or "English" without translating anything.
 */
@Composable
private fun LanguageCard(language: AppLanguage, onLanguage: (AppLanguage) -> Unit) {
    SettingsCard {
        Column {
            Text(
                stringResource(R.string.language_title),
                color = Palette.TextPrimary,
                fontSize = 15.sp,
                fontWeight = FontWeight.Medium,
            )
            Spacer(Modifier.height(12.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                LanguageChip(
                    label = stringResource(R.string.language_system),
                    selected = language == AppLanguage.SYSTEM,
                    onClick = { onLanguage(AppLanguage.SYSTEM) },
                    modifier = Modifier.weight(1.2f),
                )
                LanguageChip(
                    label = stringResource(R.string.language_chinese),
                    selected = language == AppLanguage.CHINESE,
                    onClick = { onLanguage(AppLanguage.CHINESE) },
                    modifier = Modifier.weight(1f),
                )
                LanguageChip(
                    label = stringResource(R.string.language_english),
                    selected = language == AppLanguage.ENGLISH,
                    onClick = { onLanguage(AppLanguage.ENGLISH) },
                    modifier = Modifier.weight(1f),
                )
            }
        }
    }
}

@Composable
private fun LanguageChip(
    label: String,
    selected: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val shape = RoundedCornerShape(12.dp)
    Box(
        modifier
            .height(38.dp)
            .clip(shape)
            .background(
                if (selected) Palette.Cyan.copy(alpha = 0.18f) else Color(0x14FFFFFF),
            )
            .border(
                width = 1.dp,
                color = if (selected) Palette.Cyan.copy(alpha = 0.55f) else Color.Transparent,
                shape = shape,
            )
            .clickable { onClick() },
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            color = if (selected) Palette.Cyan else Palette.TextSecondary,
            fontSize = 13.sp,
            fontWeight = if (selected) FontWeight.Medium else FontWeight.Normal,
            textAlign = TextAlign.Center,
            maxLines = 1,
        )
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

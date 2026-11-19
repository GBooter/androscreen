package com.androscreen.app.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

object Palette {
    val BackgroundTop = Color(0xFF070B18)
    val BackgroundBottom = Color(0xFF101B36)
    val Card = Color(0xFF141C33)
    val TextPrimary = Color(0xFFF3F6FF)
    val TextSecondary = Color(0xFF8C99B8)
    val Cyan = Color(0xFF5EEAD4)
    val Violet = Color(0xFFA78BFA)
    val Green = Color(0xFF4ADE80)
    val Amber = Color(0xFFFBBF24)
    val Danger = Color(0xFFF87171)
}

private val DarkColors = darkColorScheme(
    primary = Palette.Cyan,
    onPrimary = Color(0xFF04181C),
    secondary = Palette.Violet,
    onSecondary = Color(0xFF120A24),
    background = Palette.BackgroundTop,
    onBackground = Palette.TextPrimary,
    surface = Palette.Card,
    onSurface = Palette.TextPrimary,
)

@Composable
fun AndroScreenTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = DarkColors,
        typography = Typography(),
        content = content,
    )
}

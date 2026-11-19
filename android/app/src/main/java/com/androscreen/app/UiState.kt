package com.androscreen.app

/** What the activity is showing right now. */
enum class ScreenMode {
    Dashboard,
    Display,
}

data class UiState(
    val attached: Boolean = false,
    val mode: ScreenMode = ScreenMode.Dashboard,
    val videoStarted: Boolean = false,
    /**
     * The picture has arrived at least once since the screen was opened, so a
     * `videoStarted` of false from here on means the display is being rebuilt
     * for a turn, not that the screen is still starting.
     */
    val videoSeen: Boolean = false,
    /**
     * The size of the picture the computer last sent, which is not always the
     * shape the phone is held in while the virtual display catches up with a
     * turn. Zero until the first frame.
     */
    val videoWidth: Int = 0,
    val videoHeight: Int = 0,
    val notice: String? = null,
    /** The settings screen is covering the dashboard. */
    val settingsOpen: Boolean = false,
    /** Whether a plugged-in phone starts the screen by itself. */
    val autoStart: Boolean = true,
    /** Whether the phone plays the sound coming from the computer. */
    val hostAudio: Boolean = true,
    /**
     * Whether the picture is held to 1080p at 30 frames.
     *
     * For phones whose decoder cannot take the panel's full size, or whose
     * link cannot carry it smoothly; the picture is softer, but it starts.
     */
    val compatMode: Boolean = false,
)

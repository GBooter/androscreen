package com.androscreen.app.protocol

import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/** One temperature sensor of the computer, already scaled to Celsius. */
data class HostTemp(
    val label: String,
    val celsius: Float,
)

/** Host statistics sample, mirroring the daemon's JSON field names. */
data class HostStats(
    val cpuPercent: Float,
    val perCorePercent: List<Float>,
    val memTotalKib: Long,
    val memAvailableKib: Long,
    val swapTotalKib: Long,
    val swapFreeKib: Long,
    val loadOne: Float,
    val loadFive: Float,
    val loadFifteen: Float,
    val uptimeSecs: Long,
    val timestampMs: Long,
    val temps: List<HostTemp> = emptyList(),
) {
    val memUsedKib: Long get() = (memTotalKib - memAvailableKib).coerceAtLeast(0)

    val memUsedPercent: Float
        get() = if (memTotalKib <= 0L) 0f else memUsedKib.toFloat() / memTotalKib * 100f

    /** The warmest sensor, which is the one worth a headline. */
    val hottest: HostTemp? get() = temps.maxByOrNull { it.celsius }
}

/** Messages the phone receives from the host daemon. */
sealed interface HostMessage {
    data class Welcome(
        val protocol: Int,
        val hostVersion: String,
        val audio: Boolean,
        val maxScreenWidth: Int,
        val maxScreenHeight: Int,
    ) : HostMessage

    data class ScreenStarted(
        val width: Int,
        val height: Int,
        val encoder: String,
    ) : HostMessage

    data class ScreenStopped(val reason: String) : HostMessage

    data class VideoConfigured(
        val width: Int,
        val height: Int,
        val fps: Int,
        val csd: ByteArray,
    ) : HostMessage

    data class AudioConfigured(
        val sampleRate: Int,
        val channels: Int,
        val format: String,
    ) : HostMessage

    data class Stats(val stats: HostStats) : HostMessage

    data class HostError(val code: String, val message: String) : HostMessage

    data object Unknown : HostMessage
}

/** JSON control-plane encoding, matching the daemon's serde layout. */
object ControlCodec {
    const val PROTOCOL_VERSION = 1

    fun hello(appVersion: String): ByteArray = JSONObject()
        .put("type", "hello")
        .put("protocol", PROTOCOL_VERSION)
        .put("app_version", appVersion)
        .put("capabilities", JSONArray(listOf("screen", "touch", "audio")))
        .toString()
        .toByteArray(Charsets.UTF_8)

    fun startScreen(
        width: Int,
        height: Int,
        refreshHz: Int,
        audio: Boolean,
        exact: Boolean = false,
    ): ByteArray =
        JSONObject()
            .put("type", "start_screen")
            .put(
                "request",
                JSONObject()
                    .put("width", width)
                    .put("height", height)
                    .put("refresh_hz", refreshHz)
                    .put("audio", audio)
                    .put("exact", exact),
            )
            .toString()
            .toByteArray(Charsets.UTF_8)

    fun stopScreen(): ByteArray =
        JSONObject().put("type", "stop_screen").toString().toByteArray(Charsets.UTF_8)

    /**
     * Hands the USB port back to the phone.
     *
     * Accessory mode hides the phone's storage, so a user who wants the file
     * manager says so here and the host resets the port instead of holding it
     * open for a screen nobody asked for.
     */
    fun release(reason: String): ByteArray =
        JSONObject()
            .put("type", "release")
            .put("reason", reason)
            .toString()
            .toByteArray(Charsets.UTF_8)

    fun parse(payload: ByteArray): HostMessage {
        val text = String(payload, Charsets.UTF_8)
        val json = runCatching { JSONObject(text) }
            .onFailure {
                Log.w("ControlCodec", "bad control payload: ${it.message}: ${text.take(80)}")
            }
            .getOrNull() ?: return HostMessage.Unknown

        return when (json.optString("type")) {
            "welcome" -> {
                val max = json.optJSONObject("max_screen")
                HostMessage.Welcome(
                    protocol = json.optInt("protocol", PROTOCOL_VERSION),
                    hostVersion = json.optString("host_version"),
                    audio = json.optBoolean("audio", false),
                    maxScreenWidth = max?.optInt("width", 1920) ?: 1920,
                    maxScreenHeight = max?.optInt("height", 1080) ?: 1080,
                )
            }

            "screen_started" -> HostMessage.ScreenStarted(
                width = json.optInt("width"),
                height = json.optInt("height"),
                encoder = json.optString("encoder"),
            )

            "screen_stopped" -> HostMessage.ScreenStopped(json.optString("reason"))

            "video_config" -> HostMessage.VideoConfigured(
                width = json.optInt("width"),
                height = json.optInt("height"),
                fps = json.optInt("fps", 60).coerceAtLeast(1),
                csd = bytes(json.optJSONArray("csd")),
            )

            "audio_config" -> HostMessage.AudioConfigured(
                sampleRate = json.optInt("sample_rate", 48_000),
                channels = json.optInt("channels", 2),
                format = json.optString("format"),
            )

            "stats" -> json.optJSONObject("stats")
                ?.let { HostMessage.Stats(parseStats(it)) }
                ?: HostMessage.Unknown

            "error" -> HostMessage.HostError(
                code = json.optString("code"),
                message = json.optString("message"),
            )

            else -> HostMessage.Unknown
        }
    }

    private fun parseStats(json: JSONObject): HostStats {
        val cores = json.optJSONArray("per_core_percent")
        val perCore = buildList {
            if (cores != null) {
                for (index in 0 until cores.length()) {
                    add(cores.optDouble(index, 0.0).toFloat())
                }
            }
        }
        return HostStats(
            cpuPercent = json.optDouble("cpu_percent", 0.0).toFloat(),
            perCorePercent = perCore,
            memTotalKib = json.optLong("mem_total_kib"),
            memAvailableKib = json.optLong("mem_available_kib"),
            swapTotalKib = json.optLong("swap_total_kib"),
            swapFreeKib = json.optLong("swap_free_kib"),
            loadOne = json.optDouble("load_one", 0.0).toFloat(),
            loadFive = json.optDouble("load_five", 0.0).toFloat(),
            loadFifteen = json.optDouble("load_fifteen", 0.0).toFloat(),
            uptimeSecs = json.optLong("uptime_secs"),
            timestampMs = json.optLong("timestamp_ms"),
            temps = parseTemps(json.optJSONArray("temps")),
        )
    }

    /** Temperatures arrived in 0.4.0; an older daemon simply sends none. */
    private fun parseTemps(array: JSONArray?): List<HostTemp> {
        if (array == null) return emptyList()
        return buildList {
            for (index in 0 until array.length()) {
                val entry = array.optJSONObject(index) ?: continue
                val label = entry.optString("label").ifEmpty { "电脑" }
                // The daemon keeps whole numbers on the wire, so a sensor
                // arrives as millidegrees; reading a "celsius" field that was
                // never sent left every card at 0 °C.
                add(HostTemp(label = label, celsius = entry.optInt("milli_celsius") / 1000.0f))
            }
        }
    }

    private fun bytes(array: JSONArray?): ByteArray {
        if (array == null) return ByteArray(0)
        val out = ByteArray(array.length())
        for (index in out.indices) {
            out[index] = (array.optInt(index) and 0xFF).toByte()
        }
        return out
    }
}

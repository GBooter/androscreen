package com.androscreen.app.protocol

import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ControlTest {
    @Test
    fun helloAnnouncesSupportedCapabilities() {
        val json = JSONObject(String(ControlCodec.hello("0.1.0"), Charsets.UTF_8))
        assertEquals("hello", json.getString("type"))
        assertEquals(1, json.getInt("protocol"))
        assertEquals("0.1.0", json.getString("app_version"))
        val capabilities = json.getJSONArray("capabilities")
        assertEquals(3, capabilities.length())
        assertEquals("screen", capabilities.getString(0))
        assertEquals("touch", capabilities.getString(1))
        assertEquals("audio", capabilities.getString(2))
    }

    @Test
    fun startScreenCarriesRequest() {
        val json = JSONObject(String(ControlCodec.startScreen(2400, 1080, 90, true)))
        assertEquals("start_screen", json.getString("type"))
        val request = json.getJSONObject("request")
        assertEquals(2400, request.getInt("width"))
        assertEquals(1080, request.getInt("height"))
        assertEquals(90, request.getInt("refresh_hz"))
        assertTrue(request.getBoolean("audio"))
    }

    @Test
    fun stopScreenIsASimpleControlMessage() {
        val json = JSONObject(String(ControlCodec.stopScreen()))
        assertEquals("stop_screen", json.getString("type"))
    }

    @Test
    fun parsesWelcome() {
        val message = ControlCodec.parse(
            """
            {"type":"welcome","protocol":1,"host_version":"0.1.0","audio":true,
             "max_screen":{"width":3840,"height":2160}}
            """.trimIndent().toByteArray(),
        ) as HostMessage.Welcome

        assertEquals(1, message.protocol)
        assertEquals("0.1.0", message.hostVersion)
        assertTrue(message.audio)
        assertEquals(3840, message.maxScreenWidth)
        assertEquals(2160, message.maxScreenHeight)
    }

    @Test
    fun parsesVideoConfigWithCsd() {
        val message = ControlCodec.parse(
            """
            {"type":"video_config","codec":"h264","width":2400,"height":1080,
             "fps":60,"csd":[0,0,1,103,66,0,0,1,104]}
            """.trimIndent().toByteArray(),
        ) as HostMessage.VideoConfigured

        assertEquals(2400, message.width)
        assertEquals(1080, message.height)
        assertEquals(60, message.fps)
        assertArrayEquals(
            byteArrayOf(0, 0, 1, 103, 66, 0, 0, 1, 104),
            message.csd,
        )
    }

    @Test
    fun parsesStats() {
        val message = ControlCodec.parse(
            """
            {"type":"stats","stats":{"cpu_percent":37.5,
             "per_core_percent":[10,20.5],"mem_total_kib":16384000,
             "mem_available_kib":8192000,"swap_total_kib":0,"swap_free_kib":0,
             "load_one":1.25,"load_five":0.5,"load_fifteen":0.25,
             "uptime_secs":86400,"timestamp_ms":1700000000000}}
            """.trimIndent().toByteArray(),
        ) as HostMessage.Stats

        val stats = message.stats
        assertEquals(37.5f, stats.cpuPercent, 0.001f)
        assertEquals(listOf(10f, 20.5f), stats.perCorePercent)
        assertEquals(8192000L, stats.memUsedKib)
        assertEquals(50f, stats.memUsedPercent, 0.001f)
        assertEquals(1.25f, stats.loadOne, 0.001f)
        assertEquals(86400L, stats.uptimeSecs)
    }

    @Test
    fun unknownTypesAreIgnored() {
        val message = ControlCodec.parse(
            """{"type":"future_message","payload":42}""".toByteArray(),
        )
        assertEquals(HostMessage.Unknown, message)
    }

    @Test
    fun malformedJsonIsIgnored() {
        assertEquals(HostMessage.Unknown, ControlCodec.parse("not json".toByteArray()))
    }
}

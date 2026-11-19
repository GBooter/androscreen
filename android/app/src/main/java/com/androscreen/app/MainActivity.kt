package com.androscreen.app

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.res.Configuration
import android.content.SharedPreferences
import android.content.pm.ActivityInfo
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.DisplayMetrics
import android.util.Log
import android.view.Surface
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.androscreen.app.audio.AudioPlayer
import com.androscreen.app.link.UsbLink
import com.androscreen.app.protocol.ControlCodec
import com.androscreen.app.protocol.Frame
import com.androscreen.app.protocol.HostMessage
import com.androscreen.app.protocol.HostStats
import com.androscreen.app.ui.AndroScreenTheme
import com.androscreen.app.ui.CursorState
import com.androscreen.app.ui.DashboardScreen
import com.androscreen.app.ui.DisplayScreen
import com.androscreen.app.ui.SettingsScreen
import com.androscreen.app.video.AvcSupport
import com.androscreen.app.video.VideoPipeline
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * Owns the USB link, the H.264 decoder, and the audio sink, and switches
 * between the statistics dashboard and the secondary display.
 */
class MainActivity : ComponentActivity() {
    private lateinit var usbManager: UsbManager
    private lateinit var prefs: SharedPreferences
    private var sensorManager: SensorManager? = null

    private val uiState = mutableStateOf(UiState())
    private val statsState = mutableStateOf<HostStats?>(null)

    /**
     * Whether the computer's daemon is new enough to report temperatures.
     *
     * Without this the empty card would claim the machine has no sensors when
     * the truth is that its program is a release behind.
     */
    private val hostSendsTemperatures = mutableStateOf(false)

    /**
     * The last half minute of the warmest sensor.
     *
     * A single reading cannot tell a warm idle from a fan that just spun up, so
     * the dashboard draws this as a line under the numbers.
     */
    private val tempHistory = mutableStateListOf<Float>()

    private val mainHandler = Handler(Looper.getMainLooper())

    @Volatile
    private var link: UsbLink? = null
    @Volatile
    private var pipeline: VideoPipeline? = null
    @Volatile
    private var player: AudioPlayer? = null

    /** The pointer the host draws beside the picture instead of inside it. */
    private val cursor = CursorState()

    private var surface: Surface? = null
    private var currentAccessory: UsbAccessory? = null
    private var pendingAccessory: UsbAccessory? = null
    private var welcome: HostMessage.Welcome? = null
    private var screenRequested = false
    private var requestedPortrait = false
    private var reconfiguring = false
    private var wantsScreen = true
    /** Whether the computer's sound should come out of this phone. */
    private var hostAudio = true
    /** Whether the picture is held to a size old decoders can take. */
    private var compatMode = false
    /**
     * The last thing the decoder said before it gave up.
     *
     * A start that never delivers a picture is otherwise blamed on the
     * computer, even when the phone's own decoder refused the stream.
     */
    private var lastPipelineError: String? = null
    private var started = false
    private var destroyed = false
    private var linkGeneration = 0
    private var linkOpenedAt = 0L
    private var quickFailures = 0
    private var usbReceiverRegistered = false
    /**
     * Whether the current start has already been asked for a second time.
     *
     * A rebuild the phone never receives a picture from is worse than a slow
     * one: the timeout below would drop the whole display, and the user, who
     * only turned the phone, reads that as the app quitting on its own. One
     * more ask re-arms the host's keyframe debt; only a second failure is
     * treated as a host that is really not there.
     */
    private var startRetried = false

    private val startTimeout = Runnable { onScreenStartTimeout() }

    /**
     * Repeats the handshake until the host answers. The accessory endpoint
     * drops data written before the phone's first read request, so a single
     * hello can disappear when the app restarts while the cable stays in.
     */
    private val helloRetry = object : Runnable {
        override fun run() {
            if (destroyed || welcome != null) return
            val active = link ?: return
            active.sendControl(ControlCodec.hello(BuildConfig.VERSION_NAME))
            mainHandler.postDelayed(this, HELLO_RETRY_MS)
        }
    }

    /**
     * Reopens the accessory after the host restarts with the cable still in.
     *
     * Upgrading the daemon closes the link without a USB detach, so no
     * broadcast reaches the app; the accessory stays listed the whole time.
     */
    private val linkRetry = object : Runnable {
        override fun run() {
            if (destroyed || link != null) return
            val found = findAttachedAccessory()
            found?.let(::connect)
            if (link == null) {
                mainHandler.postDelayed(this, LINK_RETRY_MS)
            }
        }
    }

    /**
     * Looks for the accessory the system may never announce.
     *
     * A phone that switched into accessory mode without an attach broadcast
     * would otherwise sit on the dashboard saying the cable is not connected
     * while the computer waits for a greeting. The listing is cheap, and this
     * only acts while nothing else is already reaching for the link.
     */
    private val accessoryPoll = object : Runnable {
        override fun run() {
            if (destroyed || !started) return
            if (link == null && pendingAccessory == null && quickFailures == 0) {
                findAttachedAccessory()?.let(::connect)
            }
            mainHandler.postDelayed(this, ACCESSORY_POLL_MS)
        }
    }

    /**
     * Greets again when the host goes quiet.
     *
     * A live daemon answers at least once a second, on the dashboard with
     * statistics and on the secondary screen with frames, so a longer pause
     * means the daemon restarted under a cable that never came loose: the
     * phone kept its half of a link the computer already forgot.
     */
    private val linkWatchdog = object : Runnable {
        override fun run() {
            val active = link
            if (destroyed || active == null) return
            // Creating the display keeps the host away from the link for a
            // moment; `startTimeout` already reports a host that never comes
            // back, so only a screen that was already moving is revived here.
            val waiting = reconfiguring || (screenRequested && !uiState.value.videoStarted)
            if (!waiting
                && SystemClock.elapsedRealtime() - active.lastFrameAt >= LINK_SILENCE_MS
            ) {
                reviveLink()
            }
            mainHandler.postDelayed(this, WATCHDOG_INTERVAL_MS)
        }
    }

    /**
     * Keeps the extension screen in one shape while the phone lies flat.
     *
     * Gravity gives a phone on a desk no direction to turn towards, so the
     * system's rotation proposal wanders between the two shapes every couple of
     * seconds and every wander rebuilds the virtual display from scratch.
     * Pinning the orientation while the reading stays mostly vertical ends that;
     * picking the phone back up hands the choice to the user again.
     */
    private val postureWatcher = object : SensorEventListener {
        private val smoothed = FloatArray(3)
        private var settledFlat = false
        private var seenFlat = false
        private var seenSince = 0L

        fun reset() {
            smoothed.fill(0f)
            settledFlat = false
            seenFlat = false
            seenSince = 0L
        }

        override fun onSensorChanged(event: SensorEvent) {
            for (axis in smoothed.indices) {
                val sample = event.values[axis]
                smoothed[axis] += (sample - smoothed[axis]) * SMOOTHING
            }
            val across = hypot(smoothed[0], smoothed[1])
            val total = hypot(across, smoothed[2])
            // Free fall, or a device without a working accelerometer: no verdict.
            if (total < 1f) return
            val flat = across < total * FLAT_LIMIT
            val now = SystemClock.elapsedRealtime()
            if (flat != seenFlat) {
                seenFlat = flat
                seenSince = now
                return
            }
            if (flat == settledFlat || now - seenSince < POSTURE_DWELL_MS) return
            settledFlat = flat
            Log.i(
                TAG,
                if (flat) {
                    "the phone lies flat; the extension screen keeps its shape"
                } else {
                    "the phone is held up again; the extension screen follows it"
                },
            )
            requestedOrientation = if (flat) {
                ActivityInfo.SCREEN_ORIENTATION_LOCKED
            } else {
                ActivityInfo.SCREEN_ORIENTATION_FULL_USER
            }
        }

        override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) = Unit
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        usbManager = getSystemService(Context.USB_SERVICE) as UsbManager
        sensorManager = getSystemService(Context.SENSOR_SERVICE) as? SensorManager
        prefs = getSharedPreferences(PREFS, MODE_PRIVATE)
        wantsScreen = prefs.getBoolean(KEY_WANTS_SCREEN, true)
        hostAudio = prefs.getBoolean(KEY_HOST_AUDIO, true)
        compatMode = prefs.getBoolean(KEY_COMPAT_MODE, false)
        uiState.value = uiState.value.copy(
            autoStart = wantsScreen,
            hostAudio = hostAudio,
            compatMode = compatMode,
        )

        WindowCompat.setDecorFitsSystemWindows(window, false)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.attributes = window.attributes.apply {
                // The panel has a camera cutout; anything narrower than "always"
                // leaves one edge of the secondary screen unpainted.
                layoutInDisplayCutoutMode =
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
            }
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            val attributes = window.attributes
            attributes.layoutInDisplayCutoutMode =
                WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
            window.attributes = attributes
        }

        ContextCompat.registerReceiver(
            this,
            permissionReceiver,
            IntentFilter(ACTION_USB_PERMISSION),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )

        setContent {
            AndroScreenTheme {
                val state by uiState
                if (state.mode == ScreenMode.Display) {
                    DisplayScreen(
                        videoStarted = state.videoStarted,
                        videoSeen = state.videoSeen,
                        videoWidth = state.videoWidth,
                        videoHeight = state.videoHeight,
                        cursor = cursor,
                        notice = state.notice,
                        onDismissNotice = { uiState.value = uiState.value.copy(notice = null) },
                        onSurface = ::attachSurface,
                        onTouch = { payload -> link?.sendTouch(payload) },
                        onExit = {
                            stopScreen(
                                reason = "用户退出副屏",
                                notifyHost = true,
                                forgetScreen = true,
                            )
                        },
                    )
                } else if (state.settingsOpen) {
                    // Back belongs to the settings page: leaving the app from
                    // it would drop the link the user was just looking at.
                    BackHandler { uiState.value = uiState.value.copy(settingsOpen = false) }
                    SettingsScreen(
                        autoStart = state.autoStart,
                        hostAudio = state.hostAudio,
                        compatMode = state.compatMode,
                        attached = state.attached,
                        version = BuildConfig.VERSION_NAME,
                        onAutoStart = ::onAutoStartChanged,
                        onHostAudio = ::onHostAudioChanged,
                        onCompatMode = ::onCompatModeChanged,
                        onRelease = ::onReleaseUsb,
                        onBack = { uiState.value = uiState.value.copy(settingsOpen = false) },
                    )
                } else {
                    val stats by statsState
                    val sendsTemps by hostSendsTemperatures
                    DashboardScreen(
                        stats = stats,
                        tempHistory = tempHistory,
                        tempsSupported = sendsTemps,
                        attached = state.attached,
                        notice = state.notice,
                        onStart = ::onStartRequested,
                        onDismissNotice = { uiState.value = uiState.value.copy(notice = null) },
                        onSettings = {
                            uiState.value = uiState.value.copy(settingsOpen = true)
                        },
                        onRelease = ::onReleaseUsb,
                    )
                }
            }
        }

        handleIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        started = true
        registerUsbReceiver()
        if (link == null) {
            findAttachedAccessory()?.let(::connect)
        } else if (welcome == null) {
            mainHandler.removeCallbacks(helloRetry)
            mainHandler.postDelayed(helloRetry, HELLO_RETRY_MS)
        } else if (wantsScreen && !screenRequested) {
            startScreen()
        }
        // Some phones switch into accessory mode without delivering the attach
        // broadcast, or deliver it before the activity exists. Listing the
        // accessory is the fallback; it does nothing while a link is up.
        mainHandler.removeCallbacks(accessoryPoll)
        mainHandler.postDelayed(accessoryPoll, ACCESSORY_POLL_MS)
    }

    override fun onStop() {
        started = false
        mainHandler.removeCallbacks(accessoryPoll)
        if (screenRequested) {
            stopScreen(reason = "手机退到后台", notifyHost = true, forgetScreen = false)
        }
        unregisterUsbReceiver()
        super.onStop()
    }

    /**
     * The activity handles its own orientation changes, so a turn re-shapes the
     * virtual display instead of restarting the app in the middle of a stream.
     */
    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        recheckTurn()
    }

    /**
     * Asks for the shape the phone is held in, once that has clearly settled.
     *
     * Rebuilding the display is expensive, so one turn must not start several
     * requests: a change seen while a rebuild is in flight is looked at again
     * when that rebuild delivers its first frame.
     */
    private fun recheckTurn() {
        if (!screenRequested || reconfiguring) return
        val portrait =
            resources.configuration.orientation == Configuration.ORIENTATION_PORTRAIT
        if (portrait == requestedPortrait) return
        // A turn that arrives before the first picture is still part of the
        // startup: the activity entering full screen is itself a configuration
        // change. Rebuilding now would race the desktop's layout of a display
        // that is not on screen yet, and a desktop asked twice at once can
        // hold the session and its own panels for the better part of a minute.
        // The first frame calls this again.
        if (!uiState.value.videoStarted) return
        mainHandler.removeCallbacks(turnSettled)
        mainHandler.postDelayed(turnSettled, TURN_SETTLE_MS)
    }

    /** A turn that never delivers a first frame must not lock out later ones. */
    private val turnWatchdog = Runnable {
        if (!reconfiguring) return@Runnable
        Log.w(TAG, "the turn produced no first frame; allowing the next one")
        reconfiguring = false
        recheckTurn()
    }

    /** The phone turned, and stayed turned; the virtual display follows it. */
    private val turnSettled = Runnable {
        if (!screenRequested || reconfiguring) return@Runnable
        val activeLink = link ?: return@Runnable
        val portrait =
            resources.configuration.orientation == Configuration.ORIENTATION_PORTRAIT
        if (portrait == requestedPortrait) return@Runnable
        Log.i(TAG, "the phone turned; the virtual display follows")
        reconfiguring = true
        // Each turn gets its own retry: the one before it may have spent the
        // allowance on a start that has since succeeded.
        startRetried = false
        // The last frame of the old shape already marked the screen as started.
        // Leaving that set makes the start timeout armed below return on its
        // first line, so a turn that never delivers a picture fails silently.
        uiState.value = uiState.value.copy(videoStarted = false)
        sendScreenRequest(activeLink)
        armStartTimeout()
        mainHandler.removeCallbacks(turnWatchdog)
        mainHandler.postDelayed(turnWatchdog, TURN_WATCHDOG_MS)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleIntent(intent)
    }

    override fun onDestroy() {
        destroyed = true
        mainHandler.removeCallbacks(startTimeout)
        mainHandler.removeCallbacks(turnSettled)
        mainHandler.removeCallbacks(turnWatchdog)
        sensorManager?.unregisterListener(postureWatcher)
        runCatching { unregisterReceiver(permissionReceiver) }
        unregisterUsbReceiver()
        mainHandler.removeCallbacks(helloRetry)
        mainHandler.removeCallbacks(linkWatchdog)
        mainHandler.removeCallbacks(accessoryPoll)
        closeCurrentLink()
        pipeline?.close()
        pipeline = null
        player?.close()
        player = null
        super.onDestroy()
    }

    private fun handleIntent(intent: Intent?) {
        if (intent?.action != UsbManager.ACTION_USB_ACCESSORY_ATTACHED) return
        val accessory = intent.accessory() ?: return
        connect(accessory)
    }

    private fun connect(accessory: UsbAccessory) {
        if (destroyed) return
        if (link != null) {
            if (sameAccessory(accessory, currentAccessory)) return
            closeCurrentLink()
            handleLinkClosed()
        }
        currentAccessory = accessory

        if (!usbManager.hasPermission(accessory)) {
            pendingAccessory = accessory
            uiState.value = uiState.value.copy(
                notice = "请在系统弹窗中允许 AndroScreen 访问 USB 设备",
            )
            runCatching { usbManager.requestPermission(accessory, permissionIntent()) }
                .onFailure {
                    uiState.value = uiState.value.copy(
                        notice = "无法请求 USB 权限：${it.message ?: "未知错误"}",
                    )
                }
            return
        }
        openAccessory(accessory)
    }

    private fun openAccessory(accessory: UsbAccessory) {
        val descriptor = runCatching { usbManager.openAccessory(accessory) }
            .onFailure { Log.w(TAG, "openAccessory failed", it) }
            .getOrNull()
        if (descriptor == null) {
            Log.w(TAG, "openAccessory returned null")
            uiState.value = uiState.value.copy(
                notice = "USB 连接暂不可用，请重新插拔数据线",
            )
            return
        }
        currentAccessory = accessory
        pendingAccessory = null

        val generation = ++linkGeneration
        val listener = object : UsbLink.Listener {
            override fun onControl(payload: ByteArray) {
                val message = ControlCodec.parse(payload)
                // Video frames are queued from this very thread, so the decoder
                // has to be rebuilt here too. A configuration handed to the main
                // thread arrives after the keyframe the host sent behind it, and
                // that keyframe is then stamped with the shape being abandoned:
                // rebuilding the pipeline afterwards clears it from the queue,
                // and the fresh decoder waits for a picture the host, streaming a
                // still desktop, has no reason to send again.
                if (message is HostMessage.VideoConfigured) applyVideoConfig(message)
                mainHandler.post {
                    if (!destroyed && generation == linkGeneration) {
                        handleControl(message)
                    }
                }
            }

            override fun onVideo(frame: Frame) {
                pipeline?.submit(frame.payload, frame.isKeyframe)
            }

            override fun onCursor(payload: ByteArray) {
                cursor.update(payload)
            }

            override fun onAudio(payload: ByteArray) {
                player?.submit(payload)
            }

            override fun onPong(awaitingHello: Boolean) {
                if (awaitingHello) {
                    mainHandler.post {
                        if (!destroyed && generation == linkGeneration) {
                            greetAgainIfForgotten()
                        }
                    }
                }
            }

            override fun onClosed() {
                mainHandler.post {
                    if (!destroyed && generation == linkGeneration) {
                        handleLinkClosed()
                    }
                }
            }
        }
        val opened = UsbLink(descriptor, listener)
        link = opened
        linkOpenedAt = SystemClock.elapsedRealtime()
        // Only the connection flag changes: replacing the whole state here
        // would close the settings page under the user's finger, twice a
        // second, whenever the link is flapping.
        uiState.value = uiState.value.copy(attached = true)
        opened.sendControl(ControlCodec.hello(BuildConfig.VERSION_NAME))
        mainHandler.removeCallbacks(helloRetry)
        mainHandler.postDelayed(helloRetry, HELLO_RETRY_MS)
        mainHandler.removeCallbacks(linkWatchdog)
        mainHandler.postDelayed(linkWatchdog, WATCHDOG_INTERVAL_MS)
        Log.i(TAG, "accessory link opened")
    }

    /**
     * Greets again when a live host says it has no session with this phone.
     *
     * A daemon restarted with the cable still in keeps answering pings but has
     * forgotten every greeting, so the phone would sit waiting for a command
     * that no longer has anyone to send it. The link itself is fine, and the
     * host is reading while it waits, so a fresh hello on the same descriptor
     * is enough; the welcome that follows brings the screen back on its own.
     */
    private fun greetAgainIfForgotten() {
        if (welcome == null) return
        Log.i(TAG, "the host restarted with the cable in; greeting again")
        welcome = null
        stopScreen(reason = "电脑端忘记了会话", notifyHost = false, forgetScreen = false)
        mainHandler.removeCallbacks(helloRetry)
        mainHandler.postDelayed(helloRetry, HELLO_RETRY_MS)
    }

    private fun reviveLink() {
        // helloRetry is already greeting a host that has not answered once.
        if (welcome == null) return
        Log.i(TAG, "the host went quiet mid-session; reopening the link")
        // A daemon that died while a write was in flight leaves this side's
        // writer parked on a pipe nobody reads, so a fresh greeting would only
        // join the queue behind it. Drop the descriptor and open a new one.
        closeCurrentLink()
        handleLinkClosed(reconnect = true)
    }

    /**
     * Rebuilds the decoder for a stream shape, on the thread the frames arrive on.
     *
     * Kept apart from `handleControl` on purpose: everything the user can see is
     * settled on the main thread, but this has to happen before the next video
     * frame is queued, and those are queued straight from the link's reader.
     */
    private fun applyVideoConfig(message: HostMessage.VideoConfigured) {
        if (message.width <= 0 || message.height <= 0) return
        // A new stream is a new picture shape, and the pointer bitmap was
        // scaled against the old one.
        cursor.clear()
        pipeline?.configure(message.width, message.height, message.fps, message.csd)
    }

    private fun handleControl(message: HostMessage) {
        // Any reply proves the greeting landed: the host always welcomes before
        // it says anything else, and the frames arrive in that order.
        mainHandler.removeCallbacks(helloRetry)
        when (message) {
            is HostMessage.Welcome -> {
                welcome = message
                hostSendsTemperatures.value = sendsTemperatures(message.hostVersion)
                uiState.value = uiState.value.copy(attached = true)
                if (wantsScreen && started) startScreen()
            }

            is HostMessage.VideoConfigured -> {
                if (message.width <= 0 || message.height <= 0) return
                // The decoder itself was already rebuilt on the reading thread,
                // in the order the host sent the frames in; only the panel's
                // bookkeeping is left for this thread.
                uiState.value = uiState.value.copy(
                    videoWidth = message.width,
                    videoHeight = message.height,
                )
            }

            is HostMessage.AudioConfigured -> {
                player?.configure(message.sampleRate, message.channels)
            }

            is HostMessage.Stats -> {
                statsState.value = message.stats
                recordTemperature(message.stats)
            }

            is HostMessage.ScreenStopped -> {
                if (!screenRequested) return
                val reason = message.reason
                stopScreen(
                    reason = "电脑端结束了画面：$reason",
                    notifyHost = false,
                    forgetScreen = false,
                )
                uiState.value = uiState.value.copy(notice = screenFailure(reason))
            }

            is HostMessage.ScreenStarted -> Unit

            is HostMessage.HostError -> {
                if (message.code != "screen_failed") {
                    uiState.value = uiState.value.copy(notice = hostError(message))
                }
            }

            HostMessage.Unknown -> Unit
        }
    }

    /**
     * Whether a daemon version reports temperatures.
     *
     * The comparison is over dotted numbers rather than strings so that 0.10.0
     * still counts as newer than 0.4.0.
     */
    private fun sendsTemperatures(version: String): Boolean {
        val want = listOf(0, 4, 0)
        val have = version.substringBefore('-').split('.')
            .map { it.toIntOrNull() ?: 0 }
        for (index in want.indices) {
            val mine = have.getOrElse(index) { 0 }
            if (mine != want[index]) return mine > want[index]
        }
        return true
    }

    /** Keeps a rolling half minute of the warmest sensor for the line chart. */
    private fun recordTemperature(stats: HostStats) {
        val hottest = stats.hottest ?: return
        tempHistory.add(hottest.celsius)
        while (tempHistory.size > TEMP_HISTORY) {
            tempHistory.removeAt(0)
        }
    }

    private fun startScreen() {
        if (!started || screenRequested) return
        val activeLink = link ?: return
        if (welcome == null) {
            wantsScreen = true
            saveWantsScreen(true)
            return
        }

        wantsScreen = true
        saveWantsScreen(true)
        screenRequested = true
        startRetried = false
        lastPipelineError = null
        pipeline?.close()
        player?.close()
        pipeline = VideoPipeline(
            onError = { message ->
                mainHandler.post {
                    if (!destroyed) onPipelineError(message)
                }
            },
            onFirstFrame = {
                mainHandler.post {
                    if (!destroyed) {
                        mainHandler.removeCallbacks(startTimeout)
                        mainHandler.removeCallbacks(turnWatchdog)
                        reconfiguring = false
                        uiState.value =
                            uiState.value.copy(videoStarted = true, videoSeen = true)
                        // A turn seen before this first picture was left alone on
                        // purpose; it is answered now, once the desktop has
                        // finished laying out the display it would have raced.
                        recheckTurn()
                    }
                }
            },
        ).also { surface?.let(it::setSurface) }
        // A phone that will not play the computer's sound has no use for a player.
        player = if (hostAudio) AudioPlayer() else null

        uiState.value = uiState.value.copy(
            attached = true,
            mode = ScreenMode.Display,
            videoStarted = false,
            videoSeen = false,
            notice = null,
        )
        enterDisplayConfiguration()
        sendScreenRequest(activeLink)
        armStartTimeout()
    }

    /** Asks the host for a display shaped like the panel as it is held now. */
    private fun sendScreenRequest(active: UsbLink) {
        val (width, height, refresh) = screenGeometry()
        requestedPortrait = width < height
        active.sendControl(
            ControlCodec.startScreen(
                width,
                height,
                refresh,
                audio = hostAudio,
                exact = compatMode,
            ),
        )
        Log.i(TAG, "requested ${width}x${height}@${refresh}Hz${if (compatMode) " (exact)" else ""}")
    }

    private fun armStartTimeout() {
        mainHandler.removeCallbacks(startTimeout)
        mainHandler.postDelayed(startTimeout, START_TIMEOUT_MS)
    }

    private fun stopScreen(reason: String, notifyHost: Boolean, forgetScreen: Boolean) {
        Log.i(TAG, "screen stopped: $reason")
        mainHandler.removeCallbacks(startTimeout)
        mainHandler.removeCallbacks(turnSettled)
        mainHandler.removeCallbacks(turnWatchdog)
        val activeLink = link
        if (notifyHost && screenRequested) {
            activeLink?.sendControl(ControlCodec.stopScreen())
        }
        screenRequested = false
        reconfiguring = false
        startRetried = false
        pipeline?.close()
        pipeline = null
        player?.close()
        player = null
        cursor.clear()
        if (forgetScreen) {
            wantsScreen = false
            saveWantsScreen(false)
            uiState.value = uiState.value.copy(autoStart = false)
        }
        if (uiState.value.mode == ScreenMode.Display) {
            exitDisplayConfiguration()
        }
        uiState.value = uiState.value.copy(
            mode = ScreenMode.Dashboard,
            videoStarted = false,
            videoSeen = false,
        )
    }

    private fun onPipelineError(message: String) {
        lastPipelineError = message
        // The pipeline rebuilds its own decoder and catches up at the next
        // keyframe, so a broken picture is worth a note rather than a torn-down
        // display: stopping here reconnects the virtual screen, and every
        // reconnect blanks the computer's own panel for a second while X moves
        // the desktop into the new screen box.
        uiState.value = uiState.value.copy(notice = message)
    }

    private fun onScreenStartTimeout() {
        if (!screenRequested || uiState.value.videoStarted) return
        val activeLink = link
        if (!startRetried && activeLink != null) {
            startRetried = true
            Log.w(TAG, "no picture arrived; asking the desktop once more")
            uiState.value = uiState.value.copy(notice = "正在重新连接画面…")
            sendScreenRequest(activeLink)
            armStartTimeout()
            return
        }
        startRetried = false
        stopScreen(reason = "电脑端没有送来画面", notifyHost = true, forgetScreen = false)
        uiState.value = uiState.value.copy(
            notice = lastPipelineError?.let { "画面没能开始：$it" }
                ?: "电脑端没有送来画面，请确认电脑上的 AndroScreen 服务正在运行",
        )
    }

    private fun onStartRequested() {
        wantsScreen = true
        saveWantsScreen(true)
        if (link != null) {
            if (welcome != null) startScreen()
            return
        }
        // A phone that has been switched into accessory mode but never got the
        // attach broadcast can still be found by listing it, and that is the
        // difference between "no cable" and "the computer is not answering".
        val found = findAttachedAccessory()
        if (found != null) {
            connect(found)
            return
        }
        uiState.value = uiState.value.copy(notice = accessoryMissingNotice())
    }

    /**
     * Says which half of the connection is missing.
     *
     * An empty accessory list is the phone's own system never seeing the
     * computer's request; a populated one means the computer's program is not
     * running. Telling the user the wrong one sends them to the wrong end of
     * the cable.
     */
    private fun accessoryMissingNotice(): String {
        val listed = runCatching { usbManager.accessoryList }.getOrNull()
        return if (listed == null) {
            "手机系统还没有识别到电脑的副屏请求。请拔插数据线重试；" +
                "如果一直如此，可能是这台手机限制了 USB 配件模式"
        } else {
            "数据线已连接，但没有找到电脑端的 AndroScreen。请确认电脑上的 androscreen 服务正在运行"
        }
    }

    /**
     * Turns the automatic takeover on or off.
     *
     * Switching it off takes effect now rather than at the next plug-in: the
     * phone is in accessory mode and its storage is hidden, so the screen is
     * stopped and the port handed back in the same breath.
     */
    private fun onAutoStartChanged(value: Boolean) {
        wantsScreen = value
        saveWantsScreen(value)
        uiState.value = uiState.value.copy(autoStart = value)
        if (!value) releaseUsb("the user switched automatic takeover off")
    }

    /**
     * Decides whether the computer's sound comes out of this phone.
     *
     * The switch lives on the status page, so the screen is already down when it
     * moves. Saying so at the next request is what makes it take effect: the
     * computer then never records its desktop at all, rather than recording it
     * for a phone that would not play it.
     */
    private fun onHostAudioChanged(value: Boolean) {
        hostAudio = value
        saveHostAudio(value)
        uiState.value = uiState.value.copy(hostAudio = value)
    }

    /**
     * Decides whether the picture is held to a size old decoders can take.
     *
     * The settings page is only reachable with no screen running, so the
     * choice is stored and used by the next request; asking for a new one from
     * here would tear down a screen the user is not looking at.
     */
    private fun onCompatModeChanged(value: Boolean) {
        compatMode = value
        saveCompatMode(value)
        uiState.value = uiState.value.copy(compatMode = value)
    }

    private fun onReleaseUsb() {
        releaseUsb("the user asked for file transfer")
    }

    /**
     * Asks the host to let go of the port.
     *
     * Accessory mode and file transfer cannot share the cable, so the only way
     * to see the phone's storage without unplugging is for the host to reset
     * the port; it then re-enumerates with whatever the phone offers.
     */
    private fun releaseUsb(reason: String) {
        val active = link
        if (active == null) {
            // Nothing holds the port, so there is nothing to give back; the
            // settings page stays where the user put it.
            uiState.value = uiState.value.copy(notice = "数据线未连接")
            return
        }
        if (screenRequested) {
            // Releasing the cable is not a decision about the next plug-in, so
            // the automatic switch keeps whatever the user set it to.
            stopScreen(reason = "释放数据线", notifyHost = true, forgetScreen = false)
        }
        active.sendControl(ControlCodec.release(reason))
        Log.i(TAG, "asked the host to release the USB port: $reason")
        uiState.value = uiState.value.copy(
            settingsOpen = false,
            notice = "已请求释放数据线，稍后在手机上选择“文件传输”即可看到存储",
        )
    }

    private fun attachSurface(newSurface: Surface?) {
        val state = if (newSurface == null) "went away" else "arrived"
        Log.i(TAG, "the display surface $state")
        surface = newSurface
        pipeline?.setSurface(newSurface)
    }

    private fun handleLinkClosed(reconnect: Boolean = false) {
        if (destroyed) return
        val hadLink = link != null || uiState.value.attached
        val wasDisplay = screenRequested || uiState.value.mode == ScreenMode.Display

        link = null
        currentAccessory = null
        pendingAccessory = null
        welcome = null
        screenRequested = false
        reconfiguring = false
        startRetried = false
        mainHandler.removeCallbacks(startTimeout)
        mainHandler.removeCallbacks(turnSettled)
        mainHandler.removeCallbacks(turnWatchdog)
        mainHandler.removeCallbacks(helloRetry)
        mainHandler.removeCallbacks(linkWatchdog)
        pipeline?.close()
        pipeline = null
        player?.close()
        player = null
        cursor.clear()
        statsState.value = null
        tempHistory.clear()
        hostSendsTemperatures.value = false
        if (uiState.value.mode == ScreenMode.Display) {
            exitDisplayConfiguration()
        }
        uiState.value = uiState.value.copy(
            attached = false,
            mode = ScreenMode.Dashboard,
            videoStarted = false,
            videoSeen = false,
            // A revive is already reaching for the daemon again; telling the
            // user to unplug the cable would only send them off doing that.
            notice = if (!reconnect && wasDisplay && hadLink) {
                "连接已断开，重新插入数据线即可恢复"
            } else {
                null
            },
        )
        if (hadLink) Log.i(TAG, "accessory link closed")
        val lived = SystemClock.elapsedRealtime() - linkOpenedAt
        quickFailures = if (hadLink && lived < QUICK_LINK_MS) {
            (quickFailures + 1).coerceAtMost(MAX_RETRY_SHIFT)
        } else {
            0
        }
        mainHandler.removeCallbacks(linkRetry)
        mainHandler.postDelayed(linkRetry, LINK_RETRY_MS shl quickFailures)
    }

    private fun closeCurrentLink() {
        linkGeneration++
        val current = link
        link = null
        current?.close()
    }

    private fun screenGeometry(): Triple<Int, Int, Int> {
        // The panel, not the app window: the bars are hidden while the display
        // runs, and a mode narrower than the panel leaves the picture
        // stretched across it instead of landing pixel for pixel.
        val (longPanel, shortPanel) = panelEdges()
        val portrait = resources.configuration.orientation == Configuration.ORIENTATION_PORTRAIT
        var width = if (portrait) shortPanel else longPanel
        var height = if (portrait) longPanel else shortPanel
        // The host's limits are written for a landscape picture; standing the
        // phone up swaps which edge needs the longer allowance. Compatibility
        // mode asks for less than either end allows, so an old decoder is never
        // handed a stream it has to refuse.
        var longEdge = min((welcome?.maxScreenWidth ?: 3840).coerceAtLeast(320), 2560)
        var shortEdge = min((welcome?.maxScreenHeight ?: 2160).coerceAtLeast(320), 1600)
        if (compatMode) {
            longEdge = min(longEdge, COMPAT_LONG_EDGE)
            shortEdge = min(shortEdge, COMPAT_SHORT_EDGE)
        }
        val scale = min(
            1f,
            min(
                longEdge.toFloat() / max(width, height),
                shortEdge.toFloat() / min(width, height),
            ),
        )
        width = ((width * scale).roundToInt() / 2 * 2).coerceAtLeast(320)
        height = ((height * scale).roundToInt() / 2 * 2).coerceAtLeast(320)

        // The panel can be larger than the decoder's own limits, so the last
        // word belongs to the codec: a request it would refuse at configure
        // time is shrunk here instead of failing after the screen went dark.
        val fitted = AvcSupport.fit(width, height)
        if (fitted != null && fitted != width to height) {
            Log.i(TAG, "the decoder cannot take ${width}x$height; asking ${fitted.first}x${fitted.second}")
            width = fitted.first
            height = fitted.second
        }

        val refresh = if (compatMode) {
            COMPAT_REFRESH_HZ
        } else {
            displayRefreshRate().roundToInt().coerceIn(30, 60)
        }
        return Triple(width, height, refresh)
    }

    /** The panel's two edges, long one first, in the pixels it really has. */
    @Suppress("DEPRECATION")
    private fun panelEdges(): Pair<Int, Int> {
        // The panel rather than the window: the display runs with the bars
        // hidden, and a screen narrower than the panel is stretched, not framed.
        val display = windowManager.defaultDisplay
        val first: Int
        val second: Int
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val mode = display.mode
            first = mode.physicalWidth
            second = mode.physicalHeight
        } else {
            val metrics = DisplayMetrics().also { display.getRealMetrics(it) }
            first = metrics.widthPixels
            second = metrics.heightPixels
        }
        return max(first, second) to min(first, second)
    }

    @Suppress("DEPRECATION")
    private fun displayRefreshRate(): Float = windowManager.defaultDisplay.refreshRate

    private fun enterDisplayConfiguration() {
        // Standing the phone up makes the extension screen tall instead of
        // wide. Following the raw sensor is the mistake though: a phone lying
        // flat on a desk has no "up", and its gravity reading wanders between
        // the two shapes every couple of seconds, rebuilding the display each
        // time. So the stream follows the user's own rotation preference, and
        // the posture watcher below pins that preference while the phone rests
        // face up or face down.
        requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_FULL_USER
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        postureWatcher.reset()
        val manager = sensorManager
        val accelerometer = manager?.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)
        if (manager != null && accelerometer != null) {
            manager.registerListener(postureWatcher, accelerometer, SensorManager.SENSOR_DELAY_UI)
        }
    }

    private fun exitDisplayConfiguration() {
        sensorManager?.unregisterListener(postureWatcher)
        requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
        WindowInsetsControllerCompat(window, window.decorView)
            .show(WindowInsetsCompat.Type.systemBars())
    }

    private fun saveWantsScreen(value: Boolean) {
        prefs.edit { putBoolean(KEY_WANTS_SCREEN, value) }
    }

    private fun saveHostAudio(value: Boolean) {
        prefs.edit { putBoolean(KEY_HOST_AUDIO, value) }
    }

    private fun saveCompatMode(value: Boolean) {
        prefs.edit { putBoolean(KEY_COMPAT_MODE, value) }
    }

    private fun findAttachedAccessory(): UsbAccessory? = runCatching {
        usbManager.accessoryList?.firstOrNull {
            it.manufacturer == AOA_MANUFACTURER && it.model == AOA_MODEL
        }
    }.getOrNull()

    private fun permissionIntent(): PendingIntent {
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            PendingIntent.FLAG_MUTABLE
        } else {
            0
        }
        return PendingIntent.getBroadcast(
            this,
            PERMISSION_REQUEST,
            Intent(ACTION_USB_PERMISSION).setPackage(packageName),
            flags or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }

    private fun screenFailure(reason: String): String? {
        if (reason.isEmpty()) return null
        return when {
            reason.contains("no evdi virtual display") ->
                "电脑端虚拟屏创建失败：找不到 evdi 虚拟显示器。请在电脑上运行 " +
                    "androscreen --evdi-scan 检查"
            reason.contains("compositor did not configure") ->
                "电脑端没有启用新屏幕，请在系统显示设置中打开 AndroScreen 后重试"
            reason.contains("ffmpeg") || reason.contains("encoder") ->
                "电脑端视频编码不可用，请确认已安装 ffmpeg"
            // The daemon describes the step that failed; repeating it is more
            // use than the generic sentence it used to be replaced with.
            else -> "电脑端虚拟屏创建失败：$reason"
        }
    }

    private fun hostError(message: HostMessage.HostError): String = when (message.code) {
        "touch_unavailable" -> "电脑端触控注入不可用，副屏仍可正常显示"
        "audio_unavailable" -> "电脑端声音转发不可用，副屏仍可正常显示"
        "screen_busy" -> "电脑端已经在投屏"
        "display_unavailable" -> screenFailure(message.message) ?: "电脑端无法创建扩展屏"
        "encoder_unavailable" -> "电脑端视频编码不可用"
        "bad_request" -> "屏幕参数不被电脑端接受"
        else -> message.message.ifEmpty { "电脑端提示：${message.code}" }
    }

    private fun registerUsbReceiver() {
        if (usbReceiverRegistered) return
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_ACCESSORY_ATTACHED)
            addAction(UsbManager.ACTION_USB_ACCESSORY_DETACHED)
        }
        ContextCompat.registerReceiver(
            this,
            usbReceiver,
            filter,
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        usbReceiverRegistered = true
    }

    private fun unregisterUsbReceiver() {
        if (!usbReceiverRegistered) return
        runCatching { unregisterReceiver(usbReceiver) }
        usbReceiverRegistered = false
    }

    private fun onAccessoryDetached(accessory: UsbAccessory?) {
        if (accessory != null && currentAccessory != null &&
            !sameAccessory(accessory, currentAccessory)
        ) {
            Log.d(TAG, "ignoring detach of a different accessory")
            return
        }
        closeCurrentLink()
        handleLinkClosed()
    }

    private val usbReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            when (intent.action) {
                UsbManager.ACTION_USB_ACCESSORY_ATTACHED ->
                    connect(intent.accessory() ?: return)

                UsbManager.ACTION_USB_ACCESSORY_DETACHED ->
                    onAccessoryDetached(intent.accessory())
            }
        }
    }

    private val permissionReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action != ACTION_USB_PERMISSION) return
            val accessory = intent.accessory() ?: pendingAccessory ?: return
            val granted = intent.getBooleanExtra(
                UsbManager.EXTRA_PERMISSION_GRANTED,
                false,
            )
            val expected = pendingAccessory
            pendingAccessory = null
            if (granted) {
                openAccessory(accessory)
            } else if (expected != null && sameAccessory(accessory, expected)) {
                uiState.value = uiState.value.copy(
                    notice = "未获得 USB 访问权限，无法连接",
                )
            }
        }
    }

    private fun Intent.accessory(): UsbAccessory? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
        } else {
            @Suppress("DEPRECATION")
            getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
        }

    /**
     * Whether two handles describe the same accessory.
     *
     * `UsbAccessory.getSerial()` throws `SecurityException` once the permission
     * is gone, which is exactly the state a detach broadcast arrives in after a
     * reinstall: reading it crashed the receiver before the link could be torn
     * down cleanly. The manufacturer/model/version triple already identifies
     * this app's only accessory, so the serial is deliberately left out.
     */
    private fun sameAccessory(first: UsbAccessory?, second: UsbAccessory?): Boolean {
        if (first == null || second == null) return first === second
        return first.manufacturer == second.manufacturer &&
            first.model == second.model &&
            first.version == second.version
    }

    private companion object {
        const val TAG = "MainActivity"
        const val PREFS = "androscreen"
        const val KEY_WANTS_SCREEN = "wants_screen"
        const val KEY_HOST_AUDIO = "host_audio"
        const val KEY_COMPAT_MODE = "compat_mode"
        const val ACTION_USB_PERMISSION = "com.androscreen.app.USB_PERMISSION"
        const val PERMISSION_REQUEST = 1
        const val START_TIMEOUT_MS = 20_000L
        const val HELLO_RETRY_MS = 1_500L
        const val LINK_RETRY_MS = 2_000L
        /** How often the accessory list is checked while no link is open. */
        const val ACCESSORY_POLL_MS = 2_000L
        /** A link that dies sooner than this was never really usable. */
        const val QUICK_LINK_MS = 2_500L
        /** Caps the backoff at 2s << 4 = 32s. */
        const val MAX_RETRY_SHIFT = 4
        const val WATCHDOG_INTERVAL_MS = 1_000L

        /** Longer than the one second between host replies, shorter than a reboot. */
        const val LINK_SILENCE_MS = 5_000L
        const val AOA_MANUFACTURER = "AndroScreen"
        const val AOA_MODEL = "VirtualDisplay"

        /** A turn must hold this long before the display is rebuilt for it. */
        const val TURN_SETTLE_MS = 250L
        /** Longest a turn may hold the pipeline before another turn is allowed. */
        const val TURN_WATCHDOG_MS = 3_000L

        /** Compatibility mode's ceiling on the picture's two edges and its rate. */
        const val COMPAT_LONG_EDGE = 1920
        const val COMPAT_SHORT_EDGE = 1080
        const val COMPAT_REFRESH_HZ = 30

        /** How much of each accelerometer reading the running average keeps. */
        const val SMOOTHING = 0.2f

        /** Gravity along the screen below this share of the total means the phone lies flat. */
        const val FLAT_LIMIT = 0.5f

        /** A posture must hold this long before the orientation is pinned or released. */
        const val POSTURE_DWELL_MS = 400L

        /** Half a minute of one-per-second temperature samples. */
        const val TEMP_HISTORY = 30
    }
}

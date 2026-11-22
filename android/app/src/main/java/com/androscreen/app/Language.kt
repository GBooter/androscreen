package com.androscreen.app

import android.content.Context
import android.content.SharedPreferences
import android.content.res.Configuration
import androidx.core.content.edit
import java.util.Locale

/**
 * The language the app draws in.
 *
 * Following the system is the default and the normal case; the other two exist
 * because a phone is shared, because someone may be reading a machine set to a
 * language they do not read, and because a translated string is easier to check
 * when it can be called up without changing the whole device.
 */
enum class AppLanguage {
    SYSTEM,
    CHINESE,
    ENGLISH;

    companion object {
        /** Reads the stored name, treating anything unknown as "follow system". */
        fun parse(value: String?): AppLanguage = when (value) {
            "zh" -> CHINESE
            "en" -> ENGLISH
            else -> SYSTEM
        }

        /** The name this choice is stored under. */
        fun code(language: AppLanguage): String? = when (language) {
            SYSTEM -> null
            CHINESE -> "zh"
            ENGLISH -> "en"
        }
    }
}

/**
 * Where the language choice lives, and how it is put in front of the UI.
 *
 * The whole activity is wrapped rather than each string being looked up by hand:
 * `stringResource` reads the resources of the context it is given, so a wrapped
 * context makes every `R.string` in Compose follow the choice with no other
 * change. That is also why switching has to recreate the activity — the context
 * is fixed once it is attached.
 */
object AppLanguageStore {
    private const val PREFS = "androscreen"
    private const val KEY_LANGUAGE = "language"
    private const val KEY_REOPEN_SETTINGS = "reopen_settings"

    /** The choice this install has stored, or "follow system". */
    fun load(context: Context): AppLanguage =
        AppLanguage.parse(prefs(context).getString(KEY_LANGUAGE, null))

    /** Stores a choice; "follow system" is stored as the absence of one. */
    fun save(context: Context, language: AppLanguage) {
        prefs(context).edit { putString(KEY_LANGUAGE, AppLanguage.code(language)) }
    }

    /**
     * The context whose resources answer in the chosen language.
     *
     * A phone left on "follow system" is handed back untouched, so the common
     * case costs nothing and keeps whatever locale the system picked.
     */
    fun wrap(context: Context): Context {
        val locale = when (load(context)) {
            AppLanguage.SYSTEM -> return context
            AppLanguage.CHINESE -> Locale.SIMPLIFIED_CHINESE
            AppLanguage.ENGLISH -> Locale.ENGLISH
        }
        val configuration = Configuration(context.resources.configuration)
        configuration.setLocale(locale)
        return context.createConfigurationContext(configuration)
    }

    /**
     * Remembers that the settings page was open across a language switch.
     *
     * Changing the language recreates the activity, and without this the user
     * who just tapped the switch would be dropped back on the dashboard and have
     * to find the page again.
     */
    fun markSettingsOpen(context: Context) {
        prefs(context).edit { putBoolean(KEY_REOPEN_SETTINGS, true) }
    }

    /** Whether the settings page was open, clearing the note as it is read. */
    fun takeSettingsOpen(context: Context): Boolean {
        val prefs = prefs(context)
        val open = prefs.getBoolean(KEY_REOPEN_SETTINGS, false)
        if (open) prefs.edit { putBoolean(KEY_REOPEN_SETTINGS, false) }
        return open
    }

    private fun prefs(context: Context): SharedPreferences =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
}

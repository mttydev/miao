package dev.miao.app.ui

import android.content.Context
import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

private val Light = lightColorScheme(
    primary = Color(0xFF3F6212),
    secondary = Color(0xFF5B5F2B),
    tertiary = Color(0xFF3E6374),
)

private val Dark = darkColorScheme(
    primary = Color(0xFFB5D680),
    secondary = Color(0xFFC4C8A0),
    tertiary = Color(0xFFA5CDE0),
)

/** Wallpaper-following color (Material You), persisted in the app preferences. */
object ThemePrefs {
    private const val KEY = "ui_dynamic_color"

    var dynamic by mutableStateOf(true)
        private set

    fun load(context: Context) {
        dynamic = prefs(context).getBoolean(KEY, true)
    }

    fun set(context: Context, value: Boolean) {
        dynamic = value
        prefs(context).edit().putBoolean(KEY, value).apply()
    }

    private fun prefs(context: Context) =
        context.getSharedPreferences("miao", Context.MODE_PRIVATE)
}

@Composable
fun MiaoTheme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val dark = isSystemInDarkTheme()
    val scheme = when {
        ThemePrefs.dynamic && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S ->
            if (dark) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        dark -> Dark
        else -> Light
    }
    MaterialTheme(colorScheme = scheme, content = content)
}

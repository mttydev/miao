package dev.miao.app

import android.app.Activity
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings

/**
 * Background survival for the session watcher. Chinese vendors kill background
 * processes aggressively, so the app points the operator at the exact switch
 * instead of failing silently after the screen locks.
 */
object KeepAlive {
    private const val KEY_DISMISSED_AT = "keepalive_dismissed_at"
    private const val DISMISS_COOLDOWN_MS = 3L * 24 * 60 * 60 * 1000

    fun ignoringBatteryOptimizations(context: Context): Boolean {
        val power = context.getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return true
        return power.isIgnoringBatteryOptimizations(context.packageName)
    }

    /** The banner reappears after a cooldown, so a dismissal is not forever. */
    fun shouldRemind(context: Context): Boolean {
        if (ignoringBatteryOptimizations(context)) return false
        val prefs = context.getSharedPreferences("miao", Context.MODE_PRIVATE)
        val dismissedAt = prefs.getLong(KEY_DISMISSED_AT, 0L)
        return System.currentTimeMillis() - dismissedAt >= DISMISS_COOLDOWN_MS
    }

    fun dismiss(context: Context) {
        context.getSharedPreferences("miao", Context.MODE_PRIVATE)
            .edit()
            .putLong(KEY_DISMISSED_AT, System.currentTimeMillis())
            .apply()
    }

    /** The system dialog is the one switch that survives vendor UI churn. */
    fun requestExemption(activity: Activity) {
        val intent = Intent(
            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
            Uri.parse("package:${activity.packageName}"),
        )
        if (!start(activity, intent)) openAppDetails(activity)
    }

    /**
     * Vendor battery/autostart pages, most specific first. Resolutions differ
     * per OEM build, so the first resolvable activity wins and everything else
     * falls back to this app's system settings page.
     */
    fun openVendorSettings(activity: Activity): Boolean {
        val candidates = listOf(
            ComponentName("com.miui.securitycenter", "com.miui.permcenter.autostart.AutoStartManagementActivity"),
            ComponentName("com.miui.securitycenter", "com.miui.powerkeeper.ui.HiddenAppsConfigActivity"),
            ComponentName("com.huawei.systemmanager", "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity"),
            ComponentName("com.huawei.systemmanager", "com.huawei.systemmanager.appcontrol.activity.StartupAppControlActivity"),
            ComponentName("com.hihonor.systemmanager", "com.hihonor.systemmanager.startupmgr.ui.StartupNormalAppListActivity"),
            ComponentName("com.oplus.battery", "com.oplus.battery.BatteryActivity"),
            ComponentName("com.coloros.safecenter", "com.coloros.safecenter.permission.startup.StartupAppListActivity"),
            ComponentName("com.oppo.safe", "com.oppo.safe.permission.startup.StartupAppListActivity"),
            ComponentName("com.vivo.permissionmanager", "com.vivo.permissionmanager.activity.BgStartUpManagerActivity"),
            ComponentName("com.iqoo.secure", "com.iqoo.secure.ui.phoneoptimize.BgStartUpManager"),
            ComponentName("com.iqoo.secure", "com.iqoo.secure.ui.phoneoptimize.AddWhiteListActivity"),
        )
        for (component in candidates) {
            val intent = Intent().setComponent(component).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            if (start(activity, intent)) return true
        }
        openAppDetails(activity)
        return false
    }

    private fun openAppDetails(activity: Activity) {
        start(
            activity,
            Intent(
                Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                Uri.parse("package:${activity.packageName}"),
            ),
        )
    }

    private fun start(activity: Activity, intent: Intent): Boolean = try {
        activity.startActivity(intent)
        true
    } catch (_: Exception) {
        false
    }
}

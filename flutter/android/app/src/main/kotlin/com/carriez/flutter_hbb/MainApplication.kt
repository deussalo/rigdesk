package com.carriez.flutter_hbb

import android.app.Application
import android.util.Log
import ffi.FFI

class MainApplication : Application() {
    internal val documentStorage by lazy { DocumentStorage(this) }

    @androidx.annotation.Keep
    fun rustDocumentStorage(request: String): String = documentStorage.request(request)

    companion object {
        private const val TAG = "MainApplication"
    }

    override fun onCreate() {
        super.onCreate()
        Log.d(TAG, "App start")
        FFI.onAppStart(applicationContext)
        dev.rig.assistant.Host.rustdeskId = { if (configLocated()) FFI.rigOwnId() else "" }
        dev.rig.assistant.Host.assignedNoTap = { configLocated() && FFI.rigOffersNoTap() }
    }

    /** Points the config at the directory Flutter recorded, as MainService does, before Rig Assistant's hello reads it. */
    private fun configLocated(): Boolean {
        val prefs = getSharedPreferences(KEY_SHARED_PREFERENCES, MODE_PRIVATE)
        return FFI.rigConfigAt(prefs.getString(KEY_APP_DIR_CONFIG_PATH, "") ?: "")
    }
}

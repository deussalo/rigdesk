// ffi.kt

package ffi

import android.content.Context
import java.nio.ByteBuffer

import com.carriez.flutter_hbb.RdClipboardManager

object FFI {
    init {
        System.loadLibrary("rustdesk")
    }

    external fun init(ctx: Context)
    external fun onAppStart(ctx: Context)
    external fun setClipboardManager(clipboardManager: RdClipboardManager)
    external fun startServer(app_dir: String, home_dir: String, custom_client_config: String)
    external fun startService()
    external fun onVideoFrameUpdate(buf: ByteBuffer)
    external fun onAudioFrameUpdate(buf: ByteBuffer)
    external fun translateLocale(localeName: String, input: String): String
    external fun refreshScreen()
    external fun closeAllSessions()
    external fun setFrameRawEnable(name: String, value: Boolean)
    external fun setCodecInfo(info: String)
    external fun getLocalOption(key: String): String
    external fun getBuildinOption(key: String): String
    external fun onClipboardUpdate(clips: ByteBuffer)
    external fun isServiceClipboardEnabled(): Boolean
    external fun cmLoginRes(connId: Int, res: Boolean): Boolean
    external fun rigKnockProved(connId: Int): Boolean
    external fun rigOneTapAccept(ask: String, agentId: String, agentTitle: String, graph: String, operator: String, lapseMs: Long): String
    external fun rigOneTapWithdraw(ask: String)
    external fun rigListening(): Boolean
    external fun invitePairKey(key: String): Boolean
    external fun inviteMint(): String
    external fun inviteMintFor(agent: String): String
    external fun inviteRevoke(): Boolean
    external fun inviteState(): String
    external fun inviteEnd(): Boolean
}

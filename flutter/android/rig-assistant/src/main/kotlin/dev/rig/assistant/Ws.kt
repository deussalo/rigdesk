package dev.rig.assistant

import android.util.Base64
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.io.EOFException
import java.io.IOException
import java.net.InetSocketAddress
import java.net.Socket
import java.net.URI
import java.security.MessageDigest
import java.security.SecureRandom
import javax.net.ssl.HttpsURLConnection
import javax.net.ssl.SSLSocket
import javax.net.ssl.SSLSocketFactory

/** A WebSocket client (RFC 6455), text frames only: Android ships none, and this keeps Rig Assistant free of dependencies. */
class Ws private constructor(private val socket: Socket) : Closeable {
    private val input = BufferedInputStream(socket.getInputStream())
    private val output = BufferedOutputStream(socket.getOutputStream())
    private val random = SecureRandom()

    private fun handshake(uri: URI, host: String, port: Int, tls: Boolean, headers: Map<String, String>) {
        val nonce = ByteArray(16).also { random.nextBytes(it) }
        val key = Base64.encodeToString(nonce, Base64.NO_WRAP)
        val path = (uri.rawPath?.ifEmpty { null } ?: "/") + (uri.rawQuery?.let { "?$it" } ?: "")
        val defaultPort = port == (if (tls) 443 else 80)
        val request = StringBuilder("GET $path HTTP/1.1\r\n")
            .append("Host: ").append(host).append(if (defaultPort) "" else ":$port").append("\r\n")
            .append("Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n")
            .append("Sec-WebSocket-Key: ").append(key).append("\r\n")
        for ((name, value) in headers) request.append(name).append(": ").append(value).append("\r\n")
        output.write(request.append("\r\n").toString().toByteArray(Charsets.UTF_8))
        output.flush()
        val status = line()
        var accept: String? = null
        var length = 0
        while (true) {
            val header = line()
            if (header.isEmpty()) break
            val colon = header.indexOf(':')
            if (colon <= 0) continue
            val name = header.substring(0, colon).trim().lowercase()
            val value = header.substring(colon + 1).trim()
            if (name == "sec-websocket-accept") accept = value
            if (name == "content-length") length = minOf(4096, value.toIntOrNull() ?: 0)
        }
        if (!status.contains(" 101 ")) {
            val body = ByteArray(length)
            var read = 0
            while (read < length) {
                val got = input.read(body, read, length - read)
                if (got < 0) break
                read += got
            }
            close()
            throw IOException("Rig refused the connection: $status" + if (read > 0) " " + String(body, 0, read, Charsets.UTF_8) else "")
        }
        val expected = Base64.encodeToString(MessageDigest.getInstance("SHA-1").digest((key + GUID).toByteArray(Charsets.UTF_8)), Base64.NO_WRAP)
        if (expected != accept) {
            close()
            throw IOException("the server's WebSocket answer is not for this request")
        }
    }

    private fun line(): String {
        val bytes = ByteArrayOutputStream()
        while (true) {
            val b = input.read()
            if (b < 0) throw EOFException("the server closed the connection during the handshake")
            if (b == '\n'.code) break
            if (b != '\r'.code) bytes.write(b)
            if (bytes.size() > 8192) throw IOException("a handshake line over 8 KiB")
        }
        return bytes.toString("UTF-8")
    }

    /** The next text message; answers pings on the way. Throws EOFException once the server closes. */
    fun receive(): String {
        val message = ByteArrayOutputStream()
        var text = false
        while (true) {
            val first = read()
            val second = read()
            val fin = first and 0x80 != 0
            val opcode = first and 0x0f
            var length = (second and 0x7f).toLong()
            if (length == 126L) length = (read().toLong() shl 8) or read().toLong()
            else if (length == 127L) {
                length = 0
                repeat(8) { length = (length shl 8) or read().toLong() }
            }
            if (length > (64L shl 20)) throw IOException("a frame over 64 MiB")
            val mask = if (second and 0x80 != 0) ByteArray(4) { read().toByte() } else null
            val payload = ByteArray(length.toInt())
            var at = 0
            while (at < payload.size) {
                val got = input.read(payload, at, payload.size - at)
                if (got < 0) throw EOFException("the server closed the connection")
                at += got
            }
            if (mask != null) for (i in payload.indices) payload[i] = (payload[i].toInt() xor mask[i % 4].toInt()).toByte()
            when (opcode) {
                0x9 -> { send(0xA, payload); continue }
                0xA -> continue
                0x8 -> {
                    try { send(0x8, ByteArray(0)) } catch (_: IOException) {}
                    throw EOFException("the server closed the connection")
                }
                0x1 -> { text = true; message.reset(); message.write(payload) }
                0x2 -> { text = false; message.reset() }
                0x0 -> if (text) message.write(payload)
                else -> throw IOException("unknown WebSocket opcode $opcode")
            }
            if (fin && text) return message.toString("UTF-8")
        }
    }

    private fun read(): Int {
        val b = input.read()
        if (b < 0) throw EOFException("the server closed the connection")
        return b
    }

    /** Blocks on the network: never call it on the main thread. */
    fun sendText(text: String) = send(0x1, text.toByteArray(Charsets.UTF_8))

    @Synchronized
    private fun send(opcode: Int, payload: ByteArray) {
        output.write(0x80 or opcode)
        when {
            payload.size < 126 -> output.write(0x80 or payload.size)
            payload.size < 65536 -> {
                output.write(0x80 or 126)
                output.write(payload.size ushr 8)
                output.write(payload.size and 0xff)
            }
            else -> {
                output.write(0x80 or 127)
                for (i in 7 downTo 0) output.write(((payload.size.toLong() ushr (8 * i)) and 0xff).toInt())
            }
        }
        val mask = ByteArray(4).also { random.nextBytes(it) }
        output.write(mask)
        output.write(ByteArray(payload.size) { i -> (payload[i].toInt() xor mask[i % 4].toInt()).toByte() })
        output.flush()
    }

    /** Closing a TLS socket writes close_notify, so this too stays off the main thread. */
    override fun close() {
        try { socket.close() } catch (_: IOException) {}
    }

    companion object {
        private const val GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        /** Dialing, TLS and the HTTP upgrade must finish within this, or the link dials again. */
        private const val HANDSHAKE_MS = 20_000
        /** Rig pings every 30 s, so a socket silent this long is dead (a network gone without a FIN): receive() throws and the link redials. */
        private const val SILENT_MS = 75_000

        fun connect(uri: URI, headers: Map<String, String>): Ws {
            val tls = uri.scheme == "wss"
            val host = uri.host
            val port = if (uri.port != -1) uri.port else if (tls) 443 else 80
            val plain = Socket()
            plain.connect(InetSocketAddress(host, port), 15_000)
            plain.soTimeout = HANDSHAKE_MS
            var socket: Socket = plain
            if (tls) {
                val secure = (SSLSocketFactory.getDefault() as SSLSocketFactory).createSocket(plain, host, port, true) as SSLSocket
                secure.startHandshake()
                if (!HttpsURLConnection.getDefaultHostnameVerifier().verify(host, secure.session)) {
                    secure.close()
                    throw IOException("the server's certificate is not for $host")
                }
                socket = secure
            }
            val ws = Ws(socket)
            try {
                ws.handshake(uri, host, port, tls, headers)
                socket.soTimeout = SILENT_MS
            } catch (e: Exception) {
                ws.close()
                throw e
            }
            return ws
        }
    }
}

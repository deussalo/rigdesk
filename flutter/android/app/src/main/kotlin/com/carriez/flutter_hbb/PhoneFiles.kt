package com.carriez.flutter_hbb

import android.content.Context
import android.os.Build
import android.os.Environment
import android.util.Base64
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.io.RandomAccessFile
import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** The phone's shared storage (/storage: the internal "Phone storage" and any SD card) for the holder, with All files access. */
class PhoneFiles(private val ctx: Context) {
    fun specs(): List<JSONObject> = listOf(
        toolSpec("files", "Files", true, "List a folder in the phone's shared storage: each entry's name, size or (dir), and when it " +
            "changed, folders first, at most $MAX_ENTRIES. A path not starting with / is under ${home()}.", emptyList(),
            "path" to toolProp("string", "The folder (default ${home()}).")),
        toolSpec("file_read", "Read file", true, "Read a file in shared storage: UTF-8 text as text, anything else as base64, from " +
            "offset, at most max_bytes (default ${READ_DEFAULT / 1024} KiB, at most ${READ_MAX / 1024} KiB); the answer says the " +
            "file's size, so read on from where it stopped.", listOf("path"),
            "path" to toolProp("string", "The file."),
            "offset" to toolProp("integer", "Where to start, in bytes (default 0)."),
            "max_bytes" to toolProp("integer", "How much to read.")),
        toolSpec("file_write", "Write file", false, "Write a file in shared storage, making its folders: text as UTF-8, or base64 " +
            "for bytes; replaces the file unless append is true. A call carries at most about 700 KiB of bytes (base64 grows them " +
            "by a third): write more with append.", listOf("path"),
            "path" to toolProp("string", "The file."),
            "text" to toolProp("string", "What to write, as text."),
            "base64" to toolProp("string", "What to write, as base64 bytes (instead of text)."),
            "append" to toolProp("boolean", "Add to the end instead of replacing.")),
    )

    fun call(name: String, args: JSONObject): String? = when (name) {
        "files" -> list(args.optString("path").trim())
        "file_read" -> read(args.optString("path").trim(), args.optLong("offset", 0), args.optInt("max_bytes", READ_DEFAULT))
        "file_write" -> write(args.optString("path").trim(), args.opt("text") as? String, args.opt("base64") as? String, args.optBoolean("append"))
        else -> null
    }

    private fun home(): String = Environment.getExternalStorageDirectory().path

    /** The path made absolute and canonical, refused unless it is in shared storage and RigDesk may reach it. */
    private fun shared(path: String): File {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) throw Refusal("the file tools need Android 11 or newer (All files access)")
        if (!Environment.isExternalStorageManager())
            throw Refusal("RigDesk has no All files access: turn it on in the phone's Settings > Apps > Special app access > All files access > RigDesk " +
                "(or adb shell appops set --uid ${ctx.packageName} MANAGE_EXTERNAL_STORAGE allow)")
        val file = (if (path.startsWith("/")) File(path) else File(home(), path)).canonicalFile
        if (!file.path.startsWith("/storage/")) throw Refusal("${file.path} is not in shared storage (/storage)")
        return file
    }

    private fun list(path: String): String {
        val dir = shared(path.ifEmpty { home() })
        if (!dir.isDirectory) throw Refusal(if (dir.exists()) "${dir.path} is a file: file_read reads it" else "no folder ${dir.path}")
        val entries = dir.listFiles() ?: throw Refusal("Android would not list ${dir.path}")
        val stamp = SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.ROOT)
        val shown = entries.sortedWith(compareBy<File>({ !it.isDirectory }, { it.name.lowercase(Locale.ROOT) })).take(MAX_ENTRIES)
        val lines = shown.map { f -> "${f.name}${if (f.isDirectory) "/" else ""}\t${if (f.isDirectory) "(dir)" else f.length().toString()}\t${stamp.format(Date(f.lastModified()))}" }
        val more = if (entries.size > MAX_ENTRIES) "\n… and ${entries.size - MAX_ENTRIES} more" else ""
        return "${dir.path}: ${entries.size} entries\n" + lines.joinToString("\n") + more
    }

    private fun read(path: String, offset: Long, maxBytes: Int): String {
        if (path.isEmpty()) throw Refusal("file_read takes a path")
        val file = shared(path)
        if (!file.isFile) throw Refusal(if (file.isDirectory) "${file.path} is a folder: files lists it" else "no file ${file.path}")
        val size = file.length()
        if (offset < 0 || offset > size) throw Refusal("offset $offset is outside the file's $size bytes")
        val want = maxBytes.coerceIn(1, READ_MAX).toLong().coerceAtMost(size - offset).toInt()
        val bytes = ByteArray(want)
        RandomAccessFile(file, "r").use { f ->
            f.seek(offset)
            f.readFully(bytes)
        }
        val text = utf8(bytes)
        val used = text?.second ?: want
        val head = "${file.path}: bytes $offset to ${offset + used} of $size" +
            if (offset + used < size) " (read on from offset ${offset + used})" else " (the end)"
        return if (text != null) "$head, as text\n${text.first}" else "$head, as base64\n" + Base64.encodeToString(bytes, Base64.NO_WRAP)
    }

    /**
     * The bytes as text when they are UTF-8, with how many of them it took: a character cut at the end of the window is
     * left for the next read.
     */
    private fun utf8(bytes: ByteArray): Pair<String, Int>? {
        val decoder = Charsets.UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT)
        for (cut in 0..3) {
            if (cut > 0 && cut >= bytes.size) break // a window of one cut character goes as base64, so a read always moves on
            try {
                val text = decoder.reset().decode(ByteBuffer.wrap(bytes, 0, bytes.size - cut)).toString()
                return if (text.any { it < ' ' && it != '\n' && it != '\r' && it != '\t' }) null else text to (bytes.size - cut)
            } catch (e: CharacterCodingException) {
                continue
            }
        }
        return null
    }

    private fun write(path: String, text: String?, base64: String?, append: Boolean): String {
        if (path.isEmpty()) throw Refusal("file_write takes a path")
        if ((text == null) == (base64 == null)) throw Refusal("file_write takes text or base64, one of them")
        val bytes = text?.toByteArray(Charsets.UTF_8) ?: try {
            Base64.decode(base64.orEmpty(), Base64.DEFAULT)
        } catch (e: IllegalArgumentException) {
            throw Refusal("base64 is not valid base64")
        }
        val file = shared(path)
        if (file.isDirectory) throw Refusal("${file.path} is a folder")
        val parent = file.parentFile ?: throw Refusal("${file.path} has no folder")
        if (!parent.isDirectory && !parent.mkdirs()) throw Refusal("could not make the folder ${parent.path}")
        FileOutputStream(file, append).use { it.write(bytes) }
        return "${if (append) "appended" else "wrote"} ${bytes.size} bytes to ${file.path}, now ${file.length()} bytes"
    }

    companion object {
        private const val MAX_ENTRIES = 500
        private const val READ_DEFAULT = 64 * 1024
        private const val READ_MAX = 1024 * 1024
    }
}

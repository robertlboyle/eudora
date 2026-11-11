package net.planet.eudora

import android.content.Context
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import androidx.activity.ComponentActivity
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import java.util.concurrent.CompletableFuture
import java.util.concurrent.atomic.AtomicReference

/**
 * Android's own file picker (the Storage Access Framework) for backups: a backup is restored from
 * wherever it is (Downloads, Drive, Files…), and a copy of one is saved anywhere, out of the
 * app's own folder (which uninstalling deletes). Called from Rust (src/android.rs) on a thread of
 * its own, which waits here until the person picks; the files are handed over as open file
 * descriptors, and Rust copies them (with counted progress).
 */
object DocumentPicker {
    private var openLauncher: ActivityResultLauncher<Array<String>>? = null
    private var createLauncher: ActivityResultLauncher<String>? = null
    private var context: Context? = null
    private val waiting = AtomicReference<CompletableFuture<Any?>?>(null)

    /** From MainActivity.onCreate (the launchers must be registered there, every time it's made). */
    fun attach(activity: ComponentActivity) {
        context = activity.applicationContext
        openLauncher = activity.registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { finish(it) }
        createLauncher = activity.registerForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) { finish(it) }
    }

    private fun finish(result: Any?) {
        waiting.getAndSet(null)?.complete(result)
    }

    /** Shows a picker (on the main thread) and waits for the answer: null when canceled. */
    private fun ask(launch: () -> Unit): Any? {
        val answer = CompletableFuture<Any?>()
        if (!waiting.compareAndSet(null, answer)) return null // (one picker at a time)
        Handler(Looper.getMainLooper()).post {
            try {
                launch()
            } catch (e: Exception) {
                finish(null)
            }
        }
        return answer.get()
    }

    /**
     * The backup files picked (several for a backup in parts): "name\u0000fd\u0000size" for each,
     * joined by \u0000; null when canceled. Each fd is Rust's to close; size is -1 when unknown.
     */
    @JvmStatic
    fun openBackups(): String? {
        val launcher = openLauncher ?: return null
        @Suppress("UNCHECKED_CAST")
        val uris = ask { launcher.launch(arrayOf("*/*")) } as? List<Uri> ?: return null
        val resolver = context?.contentResolver ?: return null
        val out = ArrayList<String>()
        for (uri in uris) {
            var name = "backup"
            var size = -1L
            resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { c ->
                if (c.moveToFirst()) {
                    if (!c.isNull(0)) name = c.getString(0)
                    if (!c.isNull(1)) size = c.getLong(1)
                }
            }
            val fd = resolver.openFileDescriptor(uri, "r")?.detachFd() ?: continue
            out += listOf(name, fd.toString(), size.toString())
        }
        return if (out.isEmpty()) null else out.joinToString("\u0000")
    }

    /** The file being saved, kept open here until Rust has written it (see `saved`). */
    private var saving: ParcelFileDescriptor? = null

    /**
     * A new file the person names and places (`name` suggested), open for writing: -1 when
     * canceled. Rust gets a copy of the descriptor; this one stays open until `saved`, because
     * closing it is what tells the place it's in (Downloads, Drive…) that the file is written:
     * without that, Downloads listed a saved backup as 0 bytes.
     */
    @JvmStatic
    fun saveAs(name: String): Int {
        val launcher = createLauncher ?: return -1
        val uri = ask { launcher.launch(name) } as? Uri ?: return -1
        val file = context?.contentResolver?.openFileDescriptor(uri, "w") ?: return -1
        saved()
        saving = file
        return file.dup().detachFd()
    }

    /** Rust has written (and closed) the file `saveAs` gave it, or given up on it. */
    @JvmStatic
    fun saved() {
        saving?.let {
            saving = null
            try {
                it.close()
            } catch (e: Exception) {
                // (nothing more to do: the bytes are written)
            }
        }
    }
}

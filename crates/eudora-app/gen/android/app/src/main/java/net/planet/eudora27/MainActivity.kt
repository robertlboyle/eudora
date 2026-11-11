package net.planet.eudora27

import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Android 15 and newer draw apps edge to edge, and the web view reports no safe-area insets
    // to the page: the page is kept clear of the status bar, the navigation bar (the gesture bar
    // ran through the tab bar's labels) and the keyboard (a message being written stays visible)
    // here. (This replaces Tauri's own listener on the same view, which padded only the top.)
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime()
      )
      view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      insets
    }
    // Android's file picker, for restoring a backup from anywhere and saving a copy of one
    DocumentPicker.attach(this)
    // new mail is looked for while Eudora is closed too (CheckWorker)
    CheckWorker.schedule(applicationContext)
  }
}

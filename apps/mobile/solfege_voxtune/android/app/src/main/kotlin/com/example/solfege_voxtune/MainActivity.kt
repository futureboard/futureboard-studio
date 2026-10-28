package com.example.solfege_voxtune

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.OpenableColumns
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel
import java.io.File
import java.io.FileOutputStream

class MainActivity : FlutterActivity() {
    private val channelName = "canto/audio"
    private val pickAudioRequestCode = 9041
    private var pendingPickResult: MethodChannel.Result? = null
    private var audioTrack: AudioTrack? = null
    private val storagePermissionRequestCode = 9042
    private var pendingDocumentsResult: MethodChannel.Result? = null
    private val micPermissionRequestCode = 9043
    private var pendingMicResult: MethodChannel.Result? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        // Projects folder: Documents/Solfege Voxtune.
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "canto/storage")
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "documentsDir" -> {
                        // Android 10 and older need the storage permission
                        // to write to shared Documents; 11+ may create
                        // their own files there without it.
                        val needsPermission =
                            Build.VERSION.SDK_INT <= Build.VERSION_CODES.Q &&
                                checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) !=
                                PackageManager.PERMISSION_GRANTED
                        if (needsPermission) {
                            pendingDocumentsResult?.success(documentsDir())
                            pendingDocumentsResult = result
                            requestPermissions(
                                arrayOf(Manifest.permission.WRITE_EXTERNAL_STORAGE),
                                storagePermissionRequestCode
                            )
                        } else {
                            result.success(documentsDir())
                        }
                    }
                    else -> result.notImplemented()
                }
            }
        // Microphone permission for live recording (asked on first use).
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "canto/mic")
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "request" -> {
                        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) ==
                            PackageManager.PERMISSION_GRANTED
                        ) {
                            result.success(true)
                        } else {
                            pendingMicResult?.success(false)
                            pendingMicResult = result
                            requestPermissions(
                                arrayOf(Manifest.permission.RECORD_AUDIO),
                                micPermissionRequestCode
                            )
                        }
                    }
                    else -> result.notImplemented()
                }
            }
        // Small persisted UI preferences (e.g. the chosen app language).
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "canto/prefs")
            .setMethodCallHandler { call, result ->
                val prefs = getSharedPreferences("canto_prefs", Context.MODE_PRIVATE)
                val key = call.argument<String>("key")
                if (key == null) {
                    result.error("ARG", "Missing key", null)
                    return@setMethodCallHandler
                }
                when (call.method) {
                    "getString" -> result.success(prefs.getString(key, null))
                    "setString" -> {
                        val value = call.argument<String>("value")
                        prefs.edit().apply {
                            if (value == null) remove(key) else putString(key, value)
                        }.apply()
                        result.success(null)
                    }
                    else -> result.notImplemented()
                }
            }
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, channelName)
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "start" -> {
                        ensureAudioTrack()
                        result.success(null)
                    }
                    "writePcm" -> {
                        val pcm = call.argument<ByteArray>("pcm")
                        if (pcm != null && pcm.isNotEmpty()) {
                            val track = ensureAudioTrack()
                            if (track.playState != AudioTrack.PLAYSTATE_PLAYING) {
                                track.play()
                            }
                            track.write(pcm, 0, pcm.size, AudioTrack.WRITE_NON_BLOCKING)
                        }
                        result.success(null)
                    }
                    "flush" -> {
                        try {
                            audioTrack?.pause()
                            audioTrack?.flush()
                            audioTrack?.play()
                        } catch (_: Exception) {
                        }
                        result.success(null)
                    }
                    "stop" -> {
                        stopAudioTrack()
                        result.success(null)
                    }
                    "pickAudioFile" -> {
                        if (pendingPickResult != null) {
                            result.error("BUSY", "File picker is already active", null)
                            return@setMethodCallHandler
                        }
                        pendingPickResult = result
                        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                            addCategory(Intent.CATEGORY_OPENABLE)
                            type = "*/*"
                            putExtra(
                                Intent.EXTRA_MIME_TYPES,
                                arrayOf(
                                    "audio/*",
                                    "application/ogg",
                                    "video/mp4"
                                )
                            )
                        }
                        startActivityForResult(intent, pickAudioRequestCode)
                    }
                    else -> result.notImplemented()
                }
            }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != pickAudioRequestCode) return
        val callback = pendingPickResult ?: return
        pendingPickResult = null

        if (resultCode != Activity.RESULT_OK || data?.data == null) {
            callback.success(null)
            return
        }

        val uri = data.data!!
        try {
            val displayName = resolveDisplayName(uri) ?: "audio.wav"
            val ext = displayName.substringAfterLast('.', "wav").lowercase()
            val safeExt = if (ext.isNotEmpty() && ext.length <= 6) ext else "wav"
            val destFile = File(cacheDir, "canto_loaded.$safeExt")

            contentResolver.openInputStream(uri)?.use { input ->
                FileOutputStream(destFile).use { output ->
                    input.copyTo(output)
                }
            } ?: run {
                callback.error("IO_ERROR", "Unable to open selected audio stream", null)
                return
            }

            callback.success(
                mapOf(
                    "path" to destFile.absolutePath,
                    "name" to displayName
                )
            )
        } catch (e: Exception) {
            callback.error("PICK_ERROR", e.message ?: "Failed to read audio file", null)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == storagePermissionRequestCode) {
            pendingDocumentsResult?.success(documentsDir())
            pendingDocumentsResult = null
        }
        if (requestCode == micPermissionRequestCode) {
            val granted = grantResults.isNotEmpty() &&
                grantResults[0] == PackageManager.PERMISSION_GRANTED
            pendingMicResult?.success(granted)
            pendingMicResult = null
        }
    }

    /** Documents/Solfege Voxtune, or the app's own Documents dir if the shared
     *  folder can't be written (permission denied, unusual storage). */
    private fun documentsDir(): String {
        val shared = File(
            Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOCUMENTS),
            "Solfege Voxtune"
        )
        if ((shared.isDirectory || shared.mkdirs()) && shared.canWrite()) {
            return shared.absolutePath
        }
        val own = File(getExternalFilesDir(Environment.DIRECTORY_DOCUMENTS) ?: filesDir, "Solfege Voxtune")
        own.mkdirs()
        return own.absolutePath
    }

    private fun resolveDisplayName(uri: Uri): String? {
        try {
            contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                ?.use { cursor ->
                    if (cursor.moveToFirst()) {
                        val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                        if (idx >= 0) {
                            return cursor.getString(idx)
                        }
                    }
                }
        } catch (_: Exception) {
        }
        return uri.lastPathSegment
    }

    private fun ensureAudioTrack(): AudioTrack {
        audioTrack?.let { return it }
        val sampleRate = 48000
        val minBuf = AudioTrack.getMinBufferSize(
            sampleRate,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        ).coerceAtLeast(4800 * 4) // ~100ms stereo i16 buffer

        val track = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(sampleRate)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .build()
            )
            .setBufferSizeInBytes(minBuf * 2)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .build()

        track.play()
        audioTrack = track
        return track
    }

    private fun stopAudioTrack() {
        try {
            audioTrack?.pause()
            audioTrack?.flush()
            audioTrack?.stop()
            audioTrack?.release()
        } catch (_: Exception) {
        }
        audioTrack = null
    }

    override fun onDestroy() {
        stopAudioTrack()
        super.onDestroy()
    }
}

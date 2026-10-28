package com.example.futureboard_mobile

import android.app.Activity
import android.content.Intent
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.net.Uri
import android.provider.OpenableColumns
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel
import java.io.File
import java.io.FileOutputStream

class MainActivity : FlutterActivity() {
    private var audioTrack: AudioTrack? = null
    private var sampleRate: Int = 48000
    private var pendingPickResult: MethodChannel.Result? = null

    companion object {
        private const val REQUEST_PICK_AUDIO = 4201
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(
            flutterEngine.dartExecutor.binaryMessenger,
            "futureboard/audio"
        ).setMethodCallHandler { call, result ->
            when (call.method) {
                "start" -> {
                    val sr = call.argument<Int>("sampleRate") ?: 48000
                    startTrack(sr)
                    result.success(sampleRate)
                }
                "writePcm" -> {
                    val data = call.argument<ByteArray>("pcm")
                    val track = audioTrack
                    if (data != null && track != null && track.playState == AudioTrack.PLAYSTATE_PLAYING) {
                        val written = track.write(data, 0, data.size, AudioTrack.WRITE_NON_BLOCKING)
                        result.success(written)
                    } else {
                        result.success(0)
                    }
                }
                "flush" -> {
                    audioTrack?.pause()
                    audioTrack?.flush()
                    audioTrack?.play()
                    result.success(null)
                }
                "stop" -> {
                    stopTrack()
                    result.success(null)
                }
                "getProjectsDir" -> {
                    val dir = File(filesDir, "FutureboardProjects")
                    if (!dir.exists()) {
                        dir.mkdirs()
                    }
                    result.success(dir.absolutePath)
                }
                "getExportPath" -> {
                    val fileName = call.argument<String>("fileName") ?: "Futureboard_Master.wav"
                    val dir = getExternalFilesDir(null) ?: filesDir
                    val out = File(dir, fileName)
                    result.success(out.absolutePath)
                }
                "pickAudioFile" -> {
                    if (pendingPickResult != null) {
                        result.error("BUSY", "File picker already active", null)
                    } else {
                        pendingPickResult = result
                        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                            addCategory(Intent.CATEGORY_OPENABLE)
                            type = "*/*"
                            putExtra(
                                Intent.EXTRA_MIME_TYPES,
                                arrayOf("audio/*", "application/octet-stream")
                            )
                        }
                        startActivityForResult(intent, REQUEST_PICK_AUDIO)
                    }
                }
                else -> result.notImplemented()
            }
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == REQUEST_PICK_AUDIO) {
            val callback = pendingPickResult
            pendingPickResult = null
            if (callback == null) return
            if (resultCode != Activity.RESULT_OK || data?.data == null) {
                callback.success(null)
                return
            }
            val uri = data.data!!
            try {
                val copied = copyUriToCache(uri)
                callback.success(copied.absolutePath)
            } catch (e: Exception) {
                callback.error("IMPORT_FAILED", e.message, null)
            }
        }
    }

    private fun copyUriToCache(uri: Uri): File {
        var displayName = "imported_audio.wav"
        contentResolver.query(uri, null, null, null, null)?.use { cursor ->
            if (cursor.moveToFirst()) {
                val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (idx >= 0) {
                    displayName = cursor.getString(idx) ?: displayName
                }
            }
        }
        val importDir = File(cacheDir, "imported_audio")
        if (!importDir.exists()) importDir.mkdirs()
        val target = File(importDir, displayName)
        contentResolver.openInputStream(uri)?.use { input ->
            FileOutputStream(target).use { output ->
                input.copyTo(output)
            }
        }
        return target
    }

    private fun startTrack(sr: Int) {
        stopTrack()
        sampleRate = sr
        val minBuf = AudioTrack.getMinBufferSize(
            sr,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        ).coerceAtLeast(sr / 5 * 4)

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
                    .setSampleRate(sr)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .build()
            )
            .setBufferSizeInBytes(minBuf * 2)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .build()
        track.play()
        audioTrack = track
    }

    private fun stopTrack() {
        audioTrack?.let {
            try {
                it.stop()
            } catch (_: Exception) {}
            it.release()
        }
        audioTrack = null
    }

    override fun onDestroy() {
        stopTrack()
        super.onDestroy()
    }
}

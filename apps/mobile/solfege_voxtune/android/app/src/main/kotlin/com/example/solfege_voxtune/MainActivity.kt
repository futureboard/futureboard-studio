package com.example.solfege_voxtune

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
    private val channelName = "canto/audio"
    private val pickAudioRequestCode = 9041
    private var pendingPickResult: MethodChannel.Result? = null
    private var audioTrack: AudioTrack? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
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

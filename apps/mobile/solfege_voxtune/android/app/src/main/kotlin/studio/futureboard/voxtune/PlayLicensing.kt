package studio.futureboard.voxtune

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Base64
import com.android.vending.licensing.ILicenseResultListener
import com.android.vending.licensing.ILicensingService
import java.security.KeyFactory
import java.security.SecureRandom
import java.security.Signature
import java.security.spec.X509EncodedKeySpec

/**
 * Asks Google Play whether this user bought the app (Play licensing), and
 * checks Play's signed answer against the app's licensing key
 * (`PLAY_LICENSE_PUBLIC_KEY` in `.env`).
 *
 * Only a signed, verified "not licensed" answer blocks the app. Anything that
 * is not an answer (no Play Store, offline, a Play error, a timeout) is
 * [State.UNKNOWN] and the app keeps working; the last real answer is kept so
 * an offline launch goes on as the last check said.
 */
class PlayLicensing(private val context: Context) {
    enum class State(val wire: String) {
        LICENSED("licensed"),
        NOT_LICENSED("not_licensed"),
        UNKNOWN("unknown"),
        DISABLED("disabled"),
    }

    private val main = Handler(Looper.getMainLooper())
    private val prefs = context.getSharedPreferences("canto_license", Context.MODE_PRIVATE)

    /** Runs one check; [done] is called once, on the main thread. */
    fun check(done: (State) -> Unit) {
        val key = BuildConfig.PLAY_LICENSE_PUBLIC_KEY
        // Test builds (PLAY_LICENSE_BYPASS) and builds without a key skip it.
        if (BuildConfig.PLAY_LICENSE_BYPASS || key.isBlank()) {
            done(State.DISABLED)
            return
        }
        val nonce = SecureRandom().nextLong()
        var finished = false
        var connection: ServiceConnection? = null

        fun finish(answer: State) {
            if (finished) return
            finished = true
            connection?.let {
                try {
                    context.unbindService(it)
                } catch (_: Exception) {
                }
            }
            done(remember(answer))
        }

        val listener = object : ILicenseResultListener.Stub() {
            override fun verifyLicense(responseCode: Int, signedData: String?, signature: String?) {
                val answer = verify(key, nonce, responseCode, signedData, signature)
                main.post { finish(answer) }
            }
        }
        connection = object : ServiceConnection {
            override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
                try {
                    ILicensingService.Stub.asInterface(service)
                        .checkLicense(nonce, context.packageName, listener)
                } catch (_: Exception) {
                    finish(State.UNKNOWN)
                }
            }

            override fun onServiceDisconnected(name: ComponentName?) {
                finish(State.UNKNOWN)
            }
        }
        val bound = try {
            context.bindService(
                Intent(SERVICE_ACTION).setPackage(PLAY_STORE),
                connection,
                Context.BIND_AUTO_CREATE,
            )
        } catch (_: Exception) {
            false
        }
        if (!bound) {
            connection = null
            finish(State.UNKNOWN)
            return
        }
        main.postDelayed({ finish(State.UNKNOWN) }, TIMEOUT_MS)
    }

    /** Opens the app's Google Play page (the store app, or the web page). */
    fun openStore() {
        val pkg = context.packageName
        val store = Intent(Intent.ACTION_VIEW, android.net.Uri.parse("market://details?id=$pkg"))
            .setPackage(PLAY_STORE)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        try {
            context.startActivity(store)
        } catch (_: Exception) {
            context.startActivity(
                Intent(
                    Intent.ACTION_VIEW,
                    android.net.Uri.parse("https://play.google.com/store/apps/details?id=$pkg"),
                ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }
    }

    /**
     * A real answer is stored; a non-answer falls back to the last real one,
     * so a bought copy keeps opening offline and an unbought one does not
     * open just by going offline.
     */
    private fun remember(answer: State): State {
        return when (answer) {
            State.LICENSED, State.NOT_LICENSED -> {
                prefs.edit().putString(LAST_KEY, answer.wire).apply()
                answer
            }
            State.UNKNOWN ->
                State.values().firstOrNull { it.wire == prefs.getString(LAST_KEY, null) }
                    ?: State.UNKNOWN
            State.DISABLED -> answer
        }
    }

    /**
     * Checks Play's response: for "licensed" and "not licensed" the signed
     * data must verify against the key and name this request (nonce), this
     * app and this version. Error codes carry no signature and are not an
     * answer.
     */
    private fun verify(
        key: String,
        nonce: Long,
        responseCode: Int,
        signedData: String?,
        signature: String?,
    ): State {
        if (responseCode != LICENSED && responseCode != NOT_LICENSED &&
            responseCode != LICENSED_OLD_KEY
        ) {
            return State.UNKNOWN
        }
        if (signedData.isNullOrEmpty() || signature.isNullOrEmpty()) return State.UNKNOWN
        val valid = try {
            val publicKey = KeyFactory.getInstance("RSA")
                .generatePublic(X509EncodedKeySpec(Base64.decode(key, Base64.DEFAULT)))
            Signature.getInstance("SHA1withRSA").run {
                initVerify(publicKey)
                update(signedData.toByteArray(Charsets.UTF_8))
                verify(Base64.decode(signature, Base64.DEFAULT))
            }
        } catch (_: Exception) {
            false
        }
        if (!valid) return State.UNKNOWN

        // responseCode|nonce|packageName|versionCode|userId|timestamp[:extras]
        val fields = signedData.substringBefore(':').split('|')
        if (fields.size < 6) return State.UNKNOWN
        val versionCode = try {
            val info = context.packageManager.getPackageInfo(context.packageName, 0)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                info.longVersionCode.toString()
            } else {
                @Suppress("DEPRECATION")
                info.versionCode.toString()
            }
        } catch (_: Exception) {
            return State.UNKNOWN
        }
        val matches = fields[0].toIntOrNull() == responseCode &&
            fields[1].toLongOrNull() == nonce &&
            fields[2] == context.packageName &&
            fields[3] == versionCode
        if (!matches) return State.UNKNOWN
        return if (responseCode == NOT_LICENSED) State.NOT_LICENSED else State.LICENSED
    }

    private companion object {
        const val SERVICE_ACTION = "com.android.vending.licensing.ILicensingService"
        const val PLAY_STORE = "com.android.vending"
        const val TIMEOUT_MS = 10_000L
        const val LAST_KEY = "last_answer"

        // Response codes from the licensing service.
        const val LICENSED = 0x0
        const val NOT_LICENSED = 0x1
        const val LICENSED_OLD_KEY = 0x2
    }
}

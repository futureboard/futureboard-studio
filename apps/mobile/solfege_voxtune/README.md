# Solfege Voxtune (Android)

Mobile vocal pitch editor. Flutter UI over `rust/` (`canto_dsp`), the
Android shell around the shared engine in `crates/SphereVoxtune`.

Google Play id: `studio.futureboard.voxtune` (it can never change once a
build is published).

## Native library

`libcanto_dsp.so` is built from `rust/` for `arm64-v8a` and `x86_64` only and
copied into `android/app/src/main/jniLibs/` (not committed). The Gradle build
filters the app to those two ABIs, so 32-bit devices are not offered it.

Needs the NDK the linkers in `rust/.cargo/config.toml` point at, and:

```bash
rustup target add aarch64-linux-android x86_64-linux-android
```

## Play upload bundle

```bash
pwsh apps/mobile/solfege_voxtune/scripts/build-play.ps1
```

builds the native library, then
`build/app/outputs/bundle/release/app-release.aab`. `-SkipRust` reuses the
libraries already in `jniLibs`.

The bundle is signed with the upload key named in `android/key.properties`
(never committed):

```properties
storeFile=C:/path/to/upload.jks
storePassword=...
keyAlias=upload
keyPassword=...
```

Without that file a release build falls back to the debug key, which runs on
a device but which Play refuses. Google Play App Signing holds the key the
app is distributed with; a lost upload key can be reset from Play Console.

## Versions

`version:` in `pubspec.yaml` is `<versionName>+<versionCode>`. The app keeps
its own version, separate from Futureboard Studio's (`scripts/sync-version.mjs`
does not touch it). The code after `+` must go up by one for every bundle
uploaded to Play.

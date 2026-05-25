# Debug keystore

`debug.keystore` here is a **deliberately-committed** Android debug
keystore. It signs debug APKs (nightly + PR builds) so that every
CI-built APK shares the same signature and Android allows in-place
upgrade instead of `INSTALL_FAILED_UPDATE_INCOMPATIBLE`.

- Alias: `androiddebugkey`
- Store/key password: `android` (standard Android default; not a secret)
- Validity: 25 years from generation
- DN: `CN=Android Debug, O=Android, C=US`

This signs **debug builds only**. There is no release signing config
in this project; do not point release builds at this keystore.

Wired up in `app/build.gradle.kts` under `signingConfigs.debug`.

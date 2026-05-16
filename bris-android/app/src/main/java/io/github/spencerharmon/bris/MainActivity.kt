package io.github.spencerharmon.bris

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.lifecycle.lifecycleScope
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import io.github.spencerharmon.bris.ui.CalibrationScreen
import io.github.spencerharmon.bris.ui.LiveScreen
import io.github.spencerharmon.bris.ui.PreUploadReviewScreen
import io.github.spencerharmon.bris.ui.SettingsScreen
import io.github.spencerharmon.bris.ui.SightLogDetailScreen
import io.github.spencerharmon.bris.ui.SightLogScreen
import io.github.spencerharmon.bris.engine.LensCatalog
import java.net.URLDecoder
import java.net.URLEncoder

/**
 * Single-activity entry point. Compose nav-graph holds the four
 * screens that exist in the spike:
 *
 *  - "live"  — camera preview + diagnostic overlay.
 *  - "calibration" — capture checkerboard frames + run calibration.
 *  - "settings" — operator preferences. Debug-mode toggle lives here.
 *  - "review" — pre-upload review screen for any of the three
 *    "send" actions (fix / calibration / debug capture).
 *
 * Debug-mode-aware affordances throughout the app are gated on
 * `Prefs.debugMode`; this activity only routes between screens.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = Prefs(this)

        setContent {
            MaterialTheme {
                Surface(modifier = Modifier) {
                    val nav = rememberNavController()
                    val debugMode by prefs.debugModeFlow.collectAsState(initial = false)
                    val selectedLensId by prefs.selectedLensIdFlow.collectAsState(initial = null)
                    val context = androidx.compose.ui.platform.LocalContext.current
                    // Resolve the effective lens id once per recomposition: the
                    // operator's saved choice if present, otherwise CameraX's
                    // default-back-camera id, otherwise the catalog fallback
                    // sentinel. This keeps the calibration-storage key stable
                    // even before the operator visits Settings.
                    val defaultBackId = androidx.compose.runtime.remember(context) {
                        LensCatalog.defaultBackCameraId(context) ?: LensCatalog.FALLBACK_LENS_ID
                    }
                    val effectiveLensId = selectedLensId ?: defaultBackId

                    NavHost(navController = nav, startDestination = "live") {
                        composable("live") {
                            LiveScreen(
                                debugMode = debugMode,
                                onOpenSettings = { nav.navigate("settings") },
                                onSendFix = { nav.navigate("review/fix") },
                                onOpenCalibration = { nav.navigate("calibration") },
                                onOpenSightLog = { nav.navigate("sight-log") },
                            )
                        }
                        composable("settings") {
                            SettingsScreen(
                                prefs = prefs,
                                onBack = { nav.popBackStack() },
                            )
                        }
                        composable("calibration") {
                            CalibrationScreen(
                                debugMode = debugMode,
                                lensId = effectiveLensId,
                                onBack = { nav.popBackStack() },
                                onSendCalibration = { nav.navigate("review/calibration") },
                            )
                        }
                        composable("review/{kind}") { backStack ->
                            val kind = backStack.arguments?.getString("kind") ?: "fix"
                            PreUploadReviewScreen(
                                kind = kind,
                                onBack = { nav.popBackStack() },
                                onSend = {
                                    // Submission orchestration is wired in
                                    // io.github.spencerharmon.bris.upload.Submitter once
                                    // an end-to-end test exists; the spike
                                    // currently logs and dismisses.
                                    nav.popBackStack()
                                },
                            )
                        }
                        composable("sight-log") {
                            SightLogScreen(
                                onBack = { nav.popBackStack() },
                                onOpen = { dirName ->
                                    val encoded = URLEncoder.encode(dirName, "UTF-8")
                                    nav.navigate("sight-log/$encoded")
                                },
                            )
                        }
                        composable("sight-log/{dir}") { backStack ->
                            val raw = backStack.arguments?.getString("dir") ?: return@composable
                            val dirName = URLDecoder.decode(raw, "UTF-8")
                            SightLogDetailScreen(
                                dirName = dirName,
                                onBack = { nav.popBackStack() },
                                onDeleted = { nav.popBackStack() },
                            )
                        }
                    }
                }
            }
        }
    }
}

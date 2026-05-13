package co.anomaly.bris

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
import co.anomaly.bris.ui.CalibrationScreen
import co.anomaly.bris.ui.LiveScreen
import co.anomaly.bris.ui.PreUploadReviewScreen
import co.anomaly.bris.ui.SettingsScreen

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

                    NavHost(navController = nav, startDestination = "live") {
                        composable("live") {
                            LiveScreen(
                                debugMode = debugMode,
                                onOpenSettings = { nav.navigate("settings") },
                                onSendFix = { nav.navigate("review/fix") },
                                onOpenCalibration = { nav.navigate("calibration") },
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
                                    // co.anomaly.bris.upload.Submitter once
                                    // an end-to-end test exists; the spike
                                    // currently logs and dismisses.
                                    nav.popBackStack()
                                },
                            )
                        }
                    }
                }
            }
        }
    }
}

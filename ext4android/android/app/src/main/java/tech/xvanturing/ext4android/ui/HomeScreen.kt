package tech.xvanturing.ext4android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.jni.Native

@Composable
fun HomeScreen() {
    val scope = rememberCoroutineScope()
    val version = remember { Native.version() }
    var running by remember { mutableStateOf(false) }
    var report by remember { mutableStateOf<String?>(null) }

    Scaffold { padding ->
        Column(
            modifier = Modifier
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(stringResource(R.string.app_name), style = MaterialTheme.typography.headlineSmall)
            Text(stringResource(R.string.native_library, version))
            Button(
                enabled = !running,
                onClick = {
                    running = true
                    scope.launch {
                        report = withContext(Dispatchers.Default) { Native.selfTest() }
                        running = false
                    }
                },
            ) {
                Text(stringResource(R.string.run_self_test))
            }
            val result = report
            if (running) {
                Text(stringResource(R.string.running))
            } else if (result != null) {
                Text(stringResource(R.string.self_test_result, result))
            }
            HorizontalDivider()
            ImageSection()
            HorizontalDivider()
            UsbSection()
        }
    }
}

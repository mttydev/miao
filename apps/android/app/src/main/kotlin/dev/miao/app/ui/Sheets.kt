package dev.miao.app.ui

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.Checkbox
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import dev.miao.app.AppModel
import dev.miao.app.HostClient
import dev.miao.app.ModelChoice
import dev.miao.app.R
import kotlinx.coroutines.launch

@Composable
fun AccountDialog(model: AppModel, onDismiss: () -> Unit) {
    var origin by rememberSaveable { mutableStateOf(model.accountURL) }
    var email by rememberSaveable { mutableStateOf("") }
    var password by rememberSaveable { mutableStateOf("") }
    var showEnrollment by rememberSaveable { mutableStateOf(false) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (model.accountSignedIn) stringResource(R.string.relay_account) else stringResource(R.string.relay_signin_title)) },
        text = {
            Column(
                modifier = Modifier.heightIn(max = 480.dp).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (!model.accountSignedIn) {
                    Text(stringResource(R.string.account_relay_url_hint, model.defaultAccountURL), fontSize = 12.sp)
                    OutlinedTextField(
                        value = origin,
                        onValueChange = { origin = it },
                        label = { Text(stringResource(R.string.account_relay_url_label)) },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    OutlinedTextField(
                        value = email,
                        onValueChange = { email = it },
                        label = { Text(stringResource(R.string.account_email)) },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    OutlinedTextField(
                        value = password,
                        onValueChange = { password = it },
                        label = { Text(stringResource(R.string.account_password)) },
                        singleLine = true,
                        visualTransformation = PasswordVisualTransformation(),
                        modifier = Modifier.fillMaxWidth(),
                    )
                    model.accountError?.let { Text(it, color = MaterialTheme.colorScheme.error, fontSize = 12.sp) }
                    Button(
                        onClick = {
                            model.signIn(origin, email, password)
                            password = ""
                        },
                        enabled = !model.accountBusy && email.isNotBlank() && password.isNotBlank(),
                    ) { Text(if (model.accountBusy) stringResource(R.string.account_signing_in) else stringResource(R.string.account_sign_in)) }
                    return@Column
                }
                Text(model.accountURL, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
                model.accountError?.let { Text(it, color = MaterialTheme.colorScheme.error, fontSize = 12.sp) }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = { model.refreshDirectory() }) { Text(stringResource(R.string.refresh_computers)) }
                    OutlinedButton(onClick = { model.signOut() }, enabled = !model.accountBusy) { Text(stringResource(R.string.account_sign_out)) }
                }
                if (model.discoveredHosts.isNotEmpty()) {
                    Text(stringResource(R.string.account_hosts_header), style = MaterialTheme.typography.titleSmall)
                    model.discoveredHosts.forEach { host ->
                        val status = if (host.online) stringResource(R.string.host_online) else stringResource(R.string.host_offline)
                        Text("${host.name} · $status", fontSize = 12.sp)
                    }
                }
                HorizontalDivider()
                Row {
                    Text(stringResource(R.string.enrollment_header), style = MaterialTheme.typography.titleSmall, modifier = Modifier.weight(1f))
                    TextButton(onClick = { showEnrollment = !showEnrollment }) {
                        Text(if (showEnrollment) stringResource(R.string.common_collapse) else stringResource(R.string.common_expand))
                    }
                }
                if (!showEnrollment) return@Column
                Row {
                    Checkbox(
                        checked = model.enrollmentSignerConsent,
                        onCheckedChange = { model.enrollmentSignerConsent = it },
                    )
                    Text(stringResource(R.string.enrollment_confirm_statement), fontSize = 12.sp)
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(
                        onClick = { model.initializeAccountSigner() },
                        enabled = model.enrollmentSignerConsent && !model.enrollmentBusy,
                    ) { Text(stringResource(R.string.enrollment_init_signer)) }
                    OutlinedButton(onClick = { model.createEnrollmentRequest() }, enabled = !model.enrollmentBusy) {
                        Text(stringResource(R.string.enrollment_gen_code))
                    }
                }
                if (model.enrollmentSigner.isNotEmpty()) {
                    Text(stringResource(R.string.enrollment_signer_key, model.enrollmentSigner), fontSize = 10.sp, fontFamily = FontFamily.Monospace)
                }
                if (model.enrollmentRequest.isNotEmpty()) {
                    Text(stringResource(R.string.enrollment_local_code), fontSize = 11.sp)
                    Text(model.enrollmentRequest, fontSize = 9.sp, fontFamily = FontFamily.Monospace)
                }
                OutlinedTextField(
                    value = model.enrollmentIncoming,
                    onValueChange = { model.enrollmentIncoming = it },
                    label = { Text(stringResource(R.string.enrollment_new_code_label)) },
                    modifier = Modifier.fillMaxWidth(),
                    maxLines = 4,
                )
                OutlinedButton(onClick = { model.approveEnrollmentRequest() }, enabled = !model.enrollmentBusy) {
                    Text(stringResource(R.string.enrollment_approve_device))
                }
                if (model.enrollmentResponse.isNotEmpty()) {
                    Text(stringResource(R.string.enrollment_result_label), fontSize = 11.sp)
                    Text(model.enrollmentResponse, fontSize = 9.sp, fontFamily = FontFamily.Monospace)
                }
                OutlinedTextField(
                    value = model.enrollmentApproved,
                    onValueChange = { model.enrollmentApproved = it },
                    label = { Text(stringResource(R.string.enrollment_paste_result)) },
                    modifier = Modifier.fillMaxWidth(),
                    maxLines = 4,
                )
                OutlinedTextField(
                    value = model.enrollmentPin,
                    onValueChange = { model.enrollmentPin = it },
                    label = { Text(stringResource(R.string.enrollment_signer_key_label)) },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                )
                Row {
                    Checkbox(
                        checked = model.enrollmentIndependentPin,
                        onCheckedChange = { model.enrollmentIndependentPin = it },
                    )
                    Text(stringResource(R.string.enrollment_verify_statement), fontSize = 12.sp)
                }
                OutlinedButton(onClick = { model.receiveEnrollmentApproval() }, enabled = !model.enrollmentBusy) {
                    Text(stringResource(R.string.enrollment_finish))
                }
                model.enrollmentError?.let { Text(it, color = MaterialTheme.colorScheme.error, fontSize = 12.sp) }
                if (model.enrollmentComplete) Text(stringResource(R.string.enrollment_done), color = MaterialTheme.colorScheme.primary, fontSize = 12.sp)
                if (model.enrollmentBusy) Text(stringResource(R.string.enrollment_busy), fontSize = 12.sp)
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_close)) } },
    )
}

@Composable
fun PairingDialog(model: AppModel, initialLink: String, onDismiss: () -> Unit) {
    var link by rememberSaveable { mutableStateOf(initialLink) }
    val defaultLabel = stringResource(R.string.pairing_default_device)
    var label by rememberSaveable { mutableStateOf(defaultLabel) }
    val scanPrompt = stringResource(R.string.pairing_scan_prompt)
    val scanLauncher = rememberLauncherForActivityResult(ScanContract()) { result ->
        result.contents?.let { link = it }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.pairing_title)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = link,
                    onValueChange = { link = it },
                    label = { Text(stringResource(R.string.pairing_link_label)) },
                    modifier = Modifier.fillMaxWidth(),
                    maxLines = 3,
                )
                OutlinedTextField(
                    value = label,
                    onValueChange = { label = it },
                    label = { Text(stringResource(R.string.pairing_device_label)) },
                    singleLine = true,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = {
                        scanLauncher.launch(
                            ScanOptions().apply {
                                setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                                setPrompt(scanPrompt)
                                setBeepEnabled(false)
                                setOrientationLocked(false)
                            },
                        )
                    }) { Text(stringResource(R.string.pairing_scan)) }
                    Button(
                        onClick = { model.pair(link, label) },
                        enabled = link.isNotBlank() && !model.pairingBusy(),
                    ) { Text(stringResource(R.string.pairing_start)) }
                }
                model.pairing?.let { Text(it, fontSize = 13.sp) }
                model.fingerprint?.let {
                    Text(stringResource(R.string.pairing_fingerprint, it), fontSize = 10.sp, fontFamily = FontFamily.Monospace)
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                model.cancelPairing()
                onDismiss()
            }) { Text(stringResource(R.string.common_cancel)) }
        },
    )
}

@Composable
fun ProvidersDialog(client: HostClient, onDismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var keyProvider by remember { mutableStateOf<String?>(null) }
    var apiKey by remember { mutableStateOf("") }

    if (keyProvider != null) {
        AlertDialog(
            onDismissRequest = { keyProvider = null },
            title = { Text(stringResource(R.string.providers_title)) },
            text = {
                Column {
                    Text(stringResource(R.string.providers_key_note), fontSize = 12.sp)
                    OutlinedTextField(
                        value = apiKey,
                        onValueChange = { apiKey = it },
                        label = { Text("API Key") },
                        visualTransformation = PasswordVisualTransformation(),
                        singleLine = true,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    val provider = keyProvider
                    keyProvider = null
                    if (provider != null) scope.launch { client.connectProviderKey(provider, apiKey) }
                    apiKey = ""
                }) { Text(stringResource(R.string.common_save)) }
            },
            dismissButton = {
                TextButton(onClick = {
                    keyProvider = null
                    apiKey = ""
                }) { Text(stringResource(R.string.common_cancel)) }
            },
        )
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.providers_list_title)) },
        text = {
            Column(
                modifier = Modifier.heightIn(max = 480.dp).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                client.providersError?.let { Text(it, color = MaterialTheme.colorScheme.error, fontSize = 12.sp) }
                val directory = client.providers
                if (directory == null) {
                    Text(stringResource(R.string.diff_loading), fontSize = 12.sp)
                    return@Column
                }
                directory.providers.forEach { provider ->
                    Card(modifier = Modifier.fillMaxWidth()) {
                        Column(Modifier.padding(10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                            Text(provider.name, style = MaterialTheme.typography.titleSmall)
                            val status = if (provider.connected) stringResource(R.string.provider_status_connected) else stringResource(R.string.provider_status_disconnected)
                            Text(
                                status + if (provider.allowsKey) stringResource(R.string.provider_has_key) else "",
                                fontSize = 11.sp,
                            )
                            if (provider.editable) {
                                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                    OutlinedButton(onClick = { keyProvider = provider.id }) { Text(stringResource(R.string.providers_connect_key)) }
                                    OutlinedButton(onClick = {
                                        scope.launch { client.disconnectProvider(provider.id) }
                                    }) { Text(stringResource(R.string.local_mcp_disconnect)) }
                                }
                            }
                        }
                    }
                }
                HorizontalDivider()
                Text(stringResource(R.string.providers_default_model), style = MaterialTheme.typography.titleSmall)
                directory.selected?.let { Text(stringResource(R.string.providers_current, it.name), fontSize = 12.sp) }
                directory.providers.forEach { provider ->
                    val models = directory.modelsOf(provider.id)
                    if (models.isEmpty()) return@forEach
                    Text(provider.name, fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    models.take(12).forEach { model: ModelChoice ->
                        val selected = directory.selected?.key == model.key
                        TextButton(onClick = {
                            scope.launch { client.setDefaultModel(model) }
                        }) { Text((if (selected) "✓ " else "") + model.name) }
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_close)) } },
    )
}

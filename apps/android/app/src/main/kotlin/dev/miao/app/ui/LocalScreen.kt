package dev.miao.app.ui

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.miao.app.R
import dev.miao.app.local.LocalAgent
import dev.miao.app.local.LocalImages
import dev.miao.app.local.LocalMessage
import dev.miao.app.local.LocalPresets
import dev.miao.app.local.LocalProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LocalScreen(agent: LocalAgent) {
    val listState = rememberLazyListState()
    var showSessions by rememberSaveable { mutableStateOf(false) }
    var showProviders by rememberSaveable { mutableStateOf(false) }
    var showSettings by rememberSaveable { mutableStateOf(false) }
    var draft by rememberSaveable { mutableStateOf("") }
    val messages = agent.messages
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var attached by remember { mutableStateOf<List<String>>(emptyList()) }
    var attaching by remember { mutableStateOf(false) }
    val picker = rememberLauncherForActivityResult(
        ActivityResultContracts.PickMultipleVisualMedia(LocalImages.MAX_IMAGES),
    ) { uris ->
        if (uris.isEmpty()) return@rememberLauncherForActivityResult
        attaching = true
        scope.launch {
            val encoded = withContext(Dispatchers.IO) {
                uris.take(LocalImages.MAX_IMAGES).mapNotNull { uri -> LocalImages.encode(context, uri) }
            }
            attached = (attached + encoded).take(LocalImages.MAX_IMAGES)
            attaching = false
        }
    }

    LaunchedEffect(messages.size) {
        if (messages.isNotEmpty()) listState.animateScrollToItem(messages.lastIndex)
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(stringResource(R.string.tab_local), maxLines = 1, overflow = TextOverflow.Ellipsis)
                        Text(
                            agent.provider.name.ifEmpty { stringResource(R.string.local_provider_none) } +
                                if (agent.ready) "" else stringResource(R.string.local_provider_needs_config),
                            fontSize = 11.sp,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                },
                actions = {
                    TextButton(onClick = { showSessions = true }) { Text(stringResource(R.string.sidebar_sessions)) }
                    TextButton(onClick = { showProviders = true }) { Text(stringResource(R.string.providers_header)) }
                    TextButton(onClick = { showSettings = true }) { Text(stringResource(R.string.common_settings)) }
                },
            )
        },
        bottomBar = {
            Surface(tonalElevation = 2.dp) {
                Column(Modifier.padding(8.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    agent.error?.let { Text(it, fontSize = 12.sp, color = MaterialTheme.colorScheme.error) }
                    if (attached.isNotEmpty() || attaching) {
                        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            if (attaching) Text(stringResource(R.string.local_attaching), fontSize = 11.sp)
                            attached.forEachIndexed { index, image ->
                                Column(horizontalAlignment = androidx.compose.ui.Alignment.CenterHorizontally) {
                                    LocalImages.decode(image)?.let { bitmap ->
                                        Image(
                                            bitmap = bitmap.asImageBitmap(),
                                            contentDescription = stringResource(R.string.attachment),
                                            contentScale = ContentScale.Crop,
                                            modifier = Modifier.size(56.dp),
                                        )
                                    }
                                    TextButton(onClick = { attached = attached.filterIndexed { i, _ -> i != index } }) {
                                        Text(stringResource(R.string.remove), fontSize = 10.sp)
                                    }
                                }
                            }
                        }
                    }
                    OutlinedTextField(
                        value = draft,
                        onValueChange = { draft = it },
                        modifier = Modifier.fillMaxWidth(),
                        placeholder = { Text(if (agent.ready) stringResource(R.string.local_placeholder_ready) else stringResource(R.string.local_placeholder_setup)) },
                        maxLines = 5,
                        enabled = agent.ready,
                    )
                    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        OutlinedButton(
                            onClick = {
                                picker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
                            },
                            enabled = agent.ready && !agent.busy && attached.size < LocalImages.MAX_IMAGES,
                        ) { Text(stringResource(R.string.composer_photo)) }
                        Button(
                            onClick = {
                                val text = draft
                                val images = attached
                                draft = ""
                                attached = emptyList()
                                agent.send(text, images)
                            },
                            enabled = agent.ready && !agent.busy && (draft.isNotBlank() || attached.isNotEmpty()),
                        ) { Text(stringResource(R.string.composer_send)) }
                        OutlinedButton(
                            onClick = { agent.retryLast() },
                            enabled = agent.ready && !agent.busy && messages.isNotEmpty(),
                        ) { Text(stringResource(R.string.common_retry)) }
                        OutlinedButton(onClick = { agent.newSession() }) { Text(stringResource(R.string.local_new_session)) }
                        Spacer(Modifier.weight(1f))
                        if (agent.busy) OutlinedButton(onClick = { agent.stop() }) { Text(stringResource(R.string.common_stop)) }
                    }
                }
            }
        },
    ) { padding ->
        LazyColumn(
            state = listState,
            modifier = Modifier.fillMaxSize().padding(padding).padding(horizontal = 10.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            if (!agent.ready) {
                item { LocalIntro(agent) }
            }
            items(messages, key = { it.id }) { message -> LocalBubble(message) }
            item { Spacer(Modifier.height(12.dp)) }
        }
    }

    if (showSessions) {
        SessionsDialog(agent, onDismiss = { showSessions = false })
    }
    if (showProviders) {
        ProvidersDialog(agent, onDismiss = { showProviders = false })
    }
    if (showSettings) {
        var mcpField by remember(agent.mcpURL) { mutableStateOf(agent.mcpURL ?: "") }
        AlertDialog(
            onDismissRequest = { showSettings = false },
            title = { Text(stringResource(R.string.local_settings_title)) },
            text = {
                Column(
                    Modifier.heightIn(max = 460.dp).verticalScroll(rememberScrollState()),
                    verticalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    val themeContext = LocalContext.current
                    Text(stringResource(R.string.local_appearance), fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Row {
                        androidx.compose.material3.Checkbox(
                            checked = ThemePrefs.dynamic,
                            onCheckedChange = { ThemePrefs.set(themeContext, it) },
                        )
                        Text(stringResource(R.string.local_dynamic_color), fontSize = 12.sp)
                    }
                    Text(stringResource(R.string.local_workspace, agent.workspace.path), fontSize = 10.sp, fontFamily = FontFamily.Monospace)
                    Row {
                        androidx.compose.material3.Checkbox(
                            checked = agent.autoAllowTools,
                            onCheckedChange = { agent.autoAllowTools = it },
                        )
                        Text(stringResource(R.string.local_auto_allow), fontSize = 12.sp)
                    }
                    OutlinedTextField(
                        value = mcpField,
                        onValueChange = { mcpField = it },
                        label = { Text(stringResource(R.string.local_mcp_label)) },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(onClick = { agent.setMcp(mcpField) }) { Text(stringResource(R.string.local_mcp_save)) }
                        OutlinedButton(
                            onClick = { agent.setMcp("") },
                            enabled = agent.mcpURL != null,
                        ) { Text(stringResource(R.string.local_mcp_disconnect)) }
                    }
                    Text(
                        when {
                            agent.mcpURL == null -> stringResource(R.string.local_mcp_none)
                            agent.mcpError != null -> agent.mcpError ?: ""
                            agent.mcpTools.isEmpty() -> stringResource(R.string.local_mcp_connecting)
                            else -> stringResource(R.string.local_mcp_connected, agent.mcpTools.size)
                        },
                        fontSize = 11.sp,
                        color = if (agent.mcpError != null) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = { showSettings = false }) { Text(stringResource(R.string.common_close)) }
            },
        )
    }
    agent.approval?.let { approval ->
        AlertDialog(
            onDismissRequest = { agent.resolve(false) },
            title = { Text(stringResource(R.string.local_approval_title, approval.tool)) },
            text = { Text(approval.detail, fontSize = 11.sp, fontFamily = FontFamily.Monospace) },
            confirmButton = { TextButton(onClick = { agent.resolve(true) }) { Text(stringResource(R.string.local_allow)) } },
            dismissButton = { TextButton(onClick = { agent.resolve(false) }) { Text(stringResource(R.string.permission_reject)) } },
        )
    }
}

@Composable
private fun LocalIntro(agent: LocalAgent) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(stringResource(R.string.local_pitch_title), style = MaterialTheme.typography.titleMedium)
            Text(
                stringResource(R.string.local_pitch_body),
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}

@Composable
private fun LocalBubble(message: LocalMessage) {
    when (message.role) {
        LocalMessage.Role.User -> Card(modifier = Modifier.fillMaxWidth()) {
            Column(Modifier.padding(10.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                message.images.forEach { image ->
                    LocalImages.decode(image)?.let { bitmap ->
                        Image(
                            bitmap = bitmap.asImageBitmap(),
                            contentDescription = stringResource(R.string.attachment),
                            contentScale = ContentScale.Fit,
                            modifier = Modifier.fillMaxWidth().heightIn(max = 220.dp),
                        )
                    }
                }
                Text(message.text, fontSize = 14.sp)
            }
        }
        LocalMessage.Role.Assistant -> Card(modifier = Modifier.fillMaxWidth()) {
            Text(message.text, fontSize = 14.sp, modifier = Modifier.padding(10.dp))
        }
        LocalMessage.Role.Tool -> Card(modifier = Modifier.fillMaxWidth()) {
            Column(Modifier.padding(10.dp)) {
                Text(stringResource(R.string.tool_label, message.toolName ?: ""), fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                Text(message.text, fontSize = 11.sp, fontFamily = FontFamily.Monospace)
            }
        }
    }
}

@Composable
private fun SessionsDialog(agent: LocalAgent, onDismiss: () -> Unit) {
    var renameID by remember { mutableStateOf<String?>(null) }
    var showRename by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.local_sessions_title)) },
        text = {
            Column(Modifier.heightIn(max = 420.dp).verticalScroll(rememberScrollState())) {
                agent.sessions.forEach { session ->
                    val current = session.id == agent.currentID
                    Row(Modifier.fillMaxWidth(), verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                        TextButton(onClick = {
                            agent.selectSession(session.id)
                            onDismiss()
                        }) {
                            Text((if (current) "✓ " else "") + (session.title.ifEmpty { stringResource(R.string.local_new_session) }))
                        }
                        Spacer(Modifier.weight(1f))
                        TextButton(onClick = {
                            renameID = session.id
                            showRename = true
                        }) { Text(stringResource(R.string.local_rename_short)) }
                        TextButton(onClick = { agent.deleteSession(session.id) }) { Text(stringResource(R.string.common_delete)) }
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                agent.newSession()
                onDismiss()
            }) { Text(stringResource(R.string.local_new_session)) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_close)) } },
    )
    val id = renameID
    if (showRename && id != null) {
        var title by remember(id) {
            mutableStateOf(agent.sessions.firstOrNull { it.id == id }?.title ?: "")
        }
        AlertDialog(
            onDismissRequest = { showRename = false },
            title = { Text(stringResource(R.string.session_rename_title)) },
            text = { OutlinedTextField(value = title, onValueChange = { title = it }, singleLine = true) },
            confirmButton = {
                TextButton(onClick = {
                    agent.renameSession(id, title)
                    showRename = false
                }) { Text(stringResource(R.string.common_save)) }
            },
            dismissButton = { TextButton(onClick = { showRename = false }) { Text(stringResource(R.string.common_cancel)) } },
        )
    }
}

@Composable
private fun ProvidersDialog(agent: LocalAgent, onDismiss: () -> Unit) {
    var editing by remember { mutableStateOf<LocalProvider?>(null) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.providers_header)) },
        text = {
            Column(Modifier.heightIn(max = 420.dp).verticalScroll(rememberScrollState())) {
                agent.providers.forEach { provider ->
                    val selected = provider.id == agent.selectedID
                    Row(Modifier.fillMaxWidth(), verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                        TextButton(onClick = { agent.select(provider.id) }) {
                            Text((if (selected) "✓ " else "") + (provider.name.ifEmpty { provider.baseURL }), maxLines = 1)
                        }
                        Spacer(Modifier.weight(1f))
                        TextButton(onClick = { editing = provider }) { Text(stringResource(R.string.common_edit)) }
                        TextButton(onClick = { agent.remove(provider.id) }) { Text(stringResource(R.string.common_delete)) }
                    }
                    Text(provider.model, fontSize = 10.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                TextButton(onClick = { editing = LocalProvider() }) { Text(stringResource(R.string.providers_add)) }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_close)) } },
    )
    editing?.let { provider ->
        ProviderEditor(agent, provider) { editing = null }
    }
}

@Composable
private fun ProviderEditor(agent: LocalAgent, initial: LocalProvider, onDismiss: () -> Unit) {
    var name by rememberSaveable(initial.id) { mutableStateOf(initial.name) }
    var baseURL by rememberSaveable(initial.id) { mutableStateOf(initial.baseURL) }
    var model by rememberSaveable(initial.id) { mutableStateOf(initial.model) }
    var apiKey by rememberSaveable(initial.id) { mutableStateOf(initial.apiKey) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (initial.name.isEmpty()) stringResource(R.string.providers_add) else stringResource(R.string.providers_edit)) },
        text = {
            Column(
                Modifier.heightIn(max = 460.dp).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text(stringResource(R.string.providers_presets), fontSize = 11.sp)
                LocalPresets.all.forEach { preset ->
                    FilterChip(
                        selected = baseURL == preset.baseURL,
                        onClick = {
                            name = preset.name
                            baseURL = preset.baseURL
                            model = preset.defaultModel
                        },
                        label = { Text(preset.name) },
                    )
                }
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text(stringResource(R.string.provider_name_label)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = baseURL,
                    onValueChange = { baseURL = it },
                    label = { Text(stringResource(R.string.provider_base_url_label)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = model,
                    onValueChange = { model = it },
                    label = { Text(stringResource(R.string.settings_models_header)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = apiKey,
                    onValueChange = { apiKey = it },
                    label = { Text("API Key") },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
                Text(stringResource(R.string.provider_key_note), fontSize = 10.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    agent.save(initial.copy(name = name, baseURL = baseURL, model = model, apiKey = apiKey))
                    onDismiss()
                },
                enabled = baseURL.isNotBlank() && model.isNotBlank(),
            ) { Text(stringResource(R.string.common_save)) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) } },
    )
}

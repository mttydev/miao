package dev.miao.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.miao.app.KeepAlive

/**
 * The reminder dsh-go taught us to copy: Chinese vendor ROMs kill background
 * processes by default, and the operator only finds out when the session stops
 * answering after the screen locks. Point at the exact switch instead.
 */
@Composable
fun KeepAliveBanner(
    activity: androidx.activity.ComponentActivity,
    onDismiss: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Card(
        modifier = modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 6.dp),
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.secondaryContainer,
        ),
    ) {
        Column(Modifier.padding(start = 14.dp, top = 10.dp, end = 6.dp, bottom = 6.dp)) {
            Text(
                "后台容易被系统清理",
                fontSize = 13.sp,
                color = MaterialTheme.colorScheme.onSecondaryContainer,
            )
            Text(
                "锁屏后可能会话停止响应。允许后台运行，或打开你手机品牌的管理页把 miao 加入白名单。",
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSecondaryContainer,
            )
            Row(
                Modifier.fillMaxWidth().padding(top = 2.dp),
                horizontalArrangement = Arrangement.End,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                TextButton(onClick = { onDismiss(); KeepAlive.dismiss(activity) }) {
                    Text("暂不", fontSize = 12.sp)
                }
                TextButton(onClick = { KeepAlive.openVendorSettings(activity) }) {
                    Text("品牌设置", fontSize = 12.sp)
                }
                TextButton(onClick = { KeepAlive.requestExemption(activity) }) {
                    Text("允许后台", fontSize = 12.sp)
                }
            }
        }
    }
}

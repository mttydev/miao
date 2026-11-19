import SwiftUI
import UIKit
import PhotosUI
import MiaoCore

@MainActor
struct RemoteView: View {
    @Bindable var model: AppModel
    @Environment(\.scenePhase) private var phase
    @Environment(\.horizontalSizeClass) private var sizeClass
    @State private var sceneID = UUID()
    @State private var hostID: UUID?
    /// Scene-scoped so a cold launch or a second iPad window returns to the
    /// Session the reader was last in, not whichever list row is first.
    @SceneStorage("miao.selectedHost") private var storedHost = ""
    @SceneStorage("miao.selectedSession") private var storedSession = ""
    @State private var sidebarAutoOpened = false
    @State private var pairing = false
    @State private var accountPresented = false
    @State private var providersHost: UUID?
    @State private var search = ""
    @State private var deleteTarget: SessionSummary?
    @State private var showArchived = false
    @State private var invitation = ""
    @State private var attached: UUID?
    @State private var attachEpoch = 0
    @State private var notificationSession: NotificationDestination?
    @State private var sidebarVisible = false
    @State private var sessionID: String?
    @State private var pendingDeepLink: DeepLinkTarget?
    @State private var creating = false
    @State private var forgetConfirmation = false
    @State private var hostToForget: AuthorizedHost?

    var body: some View {
        layout
        .task {
            if sizeClass == .regular, !sidebarAutoOpened {
                sidebarAutoOpened = true
                sidebarVisible = true
            }
            model.scene(sceneID, phase: phase)
            await model.load()
            applyNotification()
            if let value = AppTestConfiguration.invitation {
                invitation = value
                if model.accountSignedIn || AppTestConfiguration.allowLegacyTransport { pairing = true }
                else { accountPresented = true }
            }
            if hostID == nil {
                if let restored = UUID(uuidString: storedHost),
                   model.hosts.contains(where: { $0.id == restored }) {
                    hostID = restored
                } else {
                    hostID = model.hosts.first?.id
                }
            }
            if sessionID == nil, !storedSession.isEmpty { sessionID = storedSession }
            consumeDeepLink()
            await attach()
        }
        .onChange(of: hostID) { _, value in
            storedHost = value?.uuidString ?? ""
            Task { await attach() }
        }
        .onChange(of: sessionID) { _, value in storedSession = value ?? "" }
        .onChange(of: sizeClass) { _, value in
            if value == .regular, !sidebarAutoOpened {
                sidebarAutoOpened = true
                sidebarVisible = true
            }
        }
        .onChange(of: model.notificationHostID) { _, value in
            applyNotification()
        }
        .onChange(of: model.notificationDestination?.id) { _, _ in
            applyNotification()
        }
        .sheet(item: $notificationSession) { destination in
            if let client = model.clients[destination.hostID] {
                NavigationStack {
                    SessionView(client: client, session: client.session(destination.summary))
                        .toolbar { ToolbarItem(placement: .confirmationAction) { Button("关闭") { notificationSession = nil } } }
                }
            }
        }
        .sheet(isPresented: Binding(get: { providersHost != nil }, set: { if !$0 { providersHost = nil } })) {
            if let id = providersHost, let client = model.clients[id] {
                ProvidersView(client: client)
            }
        }
        .onChange(of: model.connectionEpoch) { _, _ in Task { await attach() } }
        .onChange(of: model.hosts.map(\.id)) { _, ids in
            if hostID == nil || !ids.contains(hostID!) { hostID = ids.first; sessionID = nil }
            consumeDeepLink()
        }
        .onChange(of: phase) { _, value in
            if value == .active { applyNotification() }
            model.scene(sceneID, phase: value)
            Task { if let attached { await model.clients[attached]?.scene(sceneID, phase: value) } }
        }
        .task(id: phase) {
            guard phase == .active else { return }
            var cycle = 0
            while !Task.isCancelled {
                await model.refreshDirectory()
                for client in model.clients.values where client.record.accountID != nil {
                    await client.scene(sceneID, phase: .active)
                    // The sidebar is the only consumer of list and inbox data,
                    // so skip their per-project reloads while it stays closed.
                    if client.ready, sidebarVisible || cycle % 3 == 0 {
                        await client.refreshLists()
                    }
                }
                if sidebarVisible || cycle % 3 == 0 {
                    for client in model.clients.values where client.ready {
                        await client.loadInbox()
                    }
                    await model.refreshBadge()
                }
                cycle += 1
                do { try await Task.sleep(for: .seconds(10)) } catch { return }
            }
        }
        .onDisappear {
            model.removeScene(sceneID)
            let previous = attached
            Task { if let previous { await model.clients[previous]?.removeScene(sceneID) } }
        }
        .onOpenURL { url in
            if let target = DeepLinkTarget(url: url) {
                pendingDeepLink = target
                consumeDeepLink()
                return
            }
            // The OAuth callback is consumed by ASWebAuthenticationSession.
            if url.host?.lowercased() == "auth" { return }
            invitation = url.absoluteString
            if model.accountSignedIn || AppTestConfiguration.allowLegacyTransport { pairing = true }
            else { accountPresented = true }
        }
        .overlay {
            if model.notificationOpening {
                ZStack {
                    Color.black.opacity(0.15).ignoresSafeArea()
                    ProgressView("正在打开通知…")
                        .padding(20)
                        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
                }
                .accessibilityElement(children: .combine)
            }
        }
        .sheet(isPresented: $accountPresented, onDismiss: {
            if !invitation.isEmpty, model.accountSignedIn { pairing = true }
        }) { HubAccountView(model: model) }
        .sheet(isPresented: $pairing, onDismiss: { invitation = "" }) { PairingView(model: model, initialURI: $invitation) }
        .sheet(isPresented: $creating) {
            if let hostID, let client = model.clients[hostID] {
                CreateSessionView(client: client) { created in
                    self.hostID = client.record.id
                    sessionID = created
                    withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false }
                }
            }
        }
        .confirmationDialog("从这台设备移除电脑？", isPresented: $forgetConfirmation, titleVisibility: .visible, presenting: hostToForget) { record in
            Button("移除", role: .destructive) {
                Task {
                    await model.forget(record.id)
                    if hostID == record.id, !model.hosts.contains(where: { $0.id == record.id }) {
                        hostID = model.hosts.first?.id
                        sessionID = nil
                    }
                }
            }
            Button("取消", role: .cancel) {}
        } message: { record in
            Text("将从本机移除「\(record.host.label)」及其缓存，电脑上的任务继续运行。账号电脑不会自动重新添加，可在中继账号中点击「连接我的电脑」恢复。要撤销授权，请在电脑的设备接入界面操作。")
        }
    }

    /// iPhone keeps the overlay drawer; regular-width layouts (iPad) get a real
    /// leading column so the session area is not covered.
    @ViewBuilder private var layout: some View {
        if sizeClass == .regular {
            HStack(spacing: 0) {
                if sidebarVisible { sidebarPanel }
                mainContent
            }
            .animation(.easeInOut(duration: 0.25), value: sidebarVisible)
        } else {
            ZStack {
                mainContent
                    .accessibilityHidden(sidebarVisible)
                if sidebarVisible { sidebar }
            }
        }
    }

    @ViewBuilder private var mainContent: some View {
        NavigationStack {
            Group {
                if let hostID, let client = model.clients[hostID] {
                    sessionContent(client)
                        .onAppear {
                            ensureSelection(client)
                        }
                        .onChange(of: sessionCount(client)) { _, count in
                            if count > 0 { ensureSelection(client) }
                        }
                } else {
                    ContentUnavailableView {
                        Label("随时接回会话", systemImage: "bubble.left.and.bubble.right")
                    } description: {
                        Text("登录同一账号后，这台电脑的会话会出现在侧栏里。")
                    } actions: {
                        Button("中继账号") { accountPresented = true }
                            .buttonStyle(.bordered).accessibilityIdentifier("emptyHubAccount")
                        Button("连接电脑") { pairing = true }.buttonStyle(.borderedProminent)
                    }
                }
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button {
                        withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = true }
                    } label: {
                        Image(systemName: "line.3.horizontal")
                    }.keyboardShortcut("[", modifiers: .command)
                        .accessibilityLabel("会话侧栏").accessibilityIdentifier("sessionSidebar")
                        .accessibilityValue(model.accountSignedIn
                            ? "已登录；电脑：\(model.hosts.count)；已连接：\(model.clients.values.filter { $0.ready }.count)；状态：\(model.error ?? model.accountError ?? "正常")"
                            : "未登录")
                }
            }
            .navigationTitle("miao")
            .navigationBarTitleDisplayMode(.inline)
        }
        .id((hostID?.uuidString ?? "no-host") + ":" + String(model.connectionEpoch))
    }

    @ViewBuilder private func sessionContent(_ client: HostClient) -> some View {
        if let session = activeSession(client) {
            SessionView(client: client, session: session)
            .id(session.summary.id)
        } else {
            ContentUnavailableView {
                Label("选择一个会话", systemImage: "sidebar.left")
            } description: {
                Text("点左上角打开侧栏，查看这台电脑上的会话。")
            }
        }
    }

    private func activeSession(_ client: HostClient) -> RemoteSession? {
        guard let sessionID else { return nil }
        if let cached = client.sessions[sessionID] { return cached }
        guard let summary = findSummary(client, id: sessionID) else { return nil }
        return client.session(summary)
    }

    /// A scene-restored id can outlive its Session (deleted on the computer,
    /// archived away, or on another page). Fall back only after a list exists.
    private func ensureSelection(_ client: HostClient) {
        if let sessionID, client.sessions[sessionID] != nil || findSummary(client, id: sessionID) != nil {
            return
        }
        guard let first = client.lists.values.flatMap({ $0 }).first else { return }
        sessionID = first.id
    }

    private func findSummary(_ client: HostClient, id: String) -> SessionSummary? {
        for (_, list) in client.lists {
            if let match = list.first(where: { $0.id == id }) { return match }
        }
        return nil
    }

    private func sessionCount(_ client: HostClient) -> Int {
        client.lists.values.reduce(0) { $0 + $1.count }
    }

    private var sidebar: some View {
        ZStack(alignment: .leading) {
            Color.black.opacity(0.35).ignoresSafeArea()
                .onTapGesture { withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false } }
            sidebarPanel
        }
    }

    private var sidebarPanel: some View {
        Group {
            if let hostID, let client = model.clients[hostID] {
                sidebarContent(client)
            } else {
                emptySidebar
            }
        }
        .frame(maxWidth: 330)
        .frame(maxHeight: .infinity)
        .background(.background)
        .transition(.move(edge: .leading))
    }

    @ViewBuilder private func sidebarContent(_ client: HostClient) -> some View {
        List {
            Section {
                HStack {
                    Text("电脑与会话").font(.headline)
                    Spacer()
                    Button { withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false } } label: {
                        Image(systemName: "xmark")
                    }.accessibilityLabel("关闭侧栏").buttonStyle(.plain)
                }
            }
            ForEach(model.hosts) { record in
                if let host = model.clients[record.id] {
                    Section { sidebarHeader(host) }
                    accountSessionSections(host)
                }
            }
            Section {
                Toggle("显示已归档", isOn: $showArchived)
                    .accessibilityIdentifier("showArchived")
            }
            if !pendingOperations(client).isEmpty {
                Section("待核对操作") {
                    ForEach(pendingOperations(client)) { operation in
                        VStack(alignment: .leading) {
                            Text(operation.kind.rawValue)
                            Text("结果尚未确认；不会自动重新发送").font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    Button("核对操作结果") { Task { await client.reconcileOperations() } }.disabled(!client.ready)
                }
            }
            Section { sidebarActions(client) }
        }
        .listStyle(.insetGrouped)
        .onAppear {
            Task { await client.refreshLists() }
            Task { await client.loadInbox() }
            Task { await model.refreshBadge() }
        }
        .refreshable {
            await client.refreshLists()
            await client.loadInbox()
            await model.refreshBadge()
        }
        .searchable(text: $search, prompt: "搜索会话")
        .task(id: search) {
            guard !search.isEmpty else { return }
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            await client.refreshLists(search: search)
        }
        .confirmationDialog(
            deleteTarget.map { "永久删除「\($0.title)」？" } ?? "永久删除会话？",
            isPresented: Binding(get: { deleteTarget != nil }, set: { if !$0 { deleteTarget = nil } }),
            titleVisibility: .visible
        ) {
            Button("永久删除", role: .destructive) {
                guard let session = deleteTarget else { return }
                deleteTarget = nil
                Task {
                    _ = await client.session(session).command(.sessionRemove, kind: .sessionRemove, payload: .object([:]))
                    await client.refreshLists()
                    await client.loadInbox()
                }
            }
            Button("取消", role: .cancel) { deleteTarget = nil }
        } message: {
            Text("会话记录会从电脑上删除，无法撤销。")
        }
    }

    @ViewBuilder private func sidebarHeader(_ client: HostClient) -> some View {
        HStack {
            Text(client.record.host.label).font(.headline)
            Spacer()
            Button {
                hostToForget = client.record
                forgetConfirmation = true
            } label: {
                Image(systemName: "xmark")
            }.accessibilityLabel("移除电脑：\(client.record.host.label)")
                .accessibilityIdentifier("removeHost-\(client.record.id.uuidString)").buttonStyle(.plain)
        }
        Label(client.connectionDescription, systemImage: client.ready ? "checkmark.circle.fill" : "network")
            .font(.footnote).foregroundStyle(client.ready ? Color.green : Color.secondary)
        if let error = client.error { Text(error).font(.footnote).foregroundStyle(.red) }
    }

    @ViewBuilder private func sessionSections(_ client: HostClient) -> some View {
        ForEach(client.lists.keys.sorted(), id: \.self) { project in
            sessionSection(client, project: project)
        }
    }

    @ViewBuilder private func accountSessionSections(_ client: HostClient) -> some View {
        if !client.inbox.isEmpty {
            Section("待处理 · \(client.inbox.count) 个会话 · \(client.totalPending) 项") {
                ForEach(client.inbox) { entry in
                    Button {
                        hostID = client.record.id
                        open(entry, client: client)
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(entry.title)
                            Text(entry.summary).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        }
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("inbox-\(entry.sessionID)")
                }
            }
        }
        ForEach(client.lists.keys.sorted(), id: \.self) { project in
            Section(projectName(project)) {
                ForEach(visible(client, project)) { session in
                    sessionRow(client, session: session, project: project)
                }
            }
        }
    }

    private func sessionSection(_ client: HostClient, project: String) -> some View {
        Section(projectName(project)) {
            ForEach(visible(client, project)) { session in
                sessionRow(client, session: session, project: project)
            }
        }
    }

    private func sessionRow(_ client: HostClient, session: SessionSummary, project: String?) -> some View {
        let active = sessionID == session.id
        let pending = client.pendingCount(session.id)
        let muted = model.isMuted(session.id)
        let liveRunning = client.sessions[session.id]?.pending["execution"]?["type"]?.string == "running"
        return Button {
            hostID = client.record.id
            select(session)
        } label: {
            HStack(spacing: 10) {
                Image(systemName: "bubble.left.and.text.bubble.right")
                    .foregroundStyle(active ? Color.accentColor : Color.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 4) {
                        if session.pinned {
                            Image(systemName: "pin.fill").font(.caption2).foregroundStyle(.orange)
                        }
                        Text(session.title)
                            .font(active ? Font.body.weight(.semibold) : Font.body)
                            .lineLimit(2)
                    }
                    HStack(spacing: 6) {
                        if let updated = session.updated {
                            Text(updated, format: .relative(presentation: .named))
                        }
                        if session.archived { Text("已归档") }
                    }
                    .font(.caption2).foregroundStyle(.secondary)
                }
                Spacer(minLength: 4)
                if liveRunning {
                    ProgressView().controlSize(.mini)
                        .accessibilityLabel("正在执行")
                }
                if muted {
                    Image(systemName: "bell.slash")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .accessibilityLabel("已静音此会话的待办提醒")
                } else if pending > 0 {
                    Text("\(pending)")
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(.orange)
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(Color.orange.opacity(0.16), in: Capsule())
                        .accessibilityLabel("\(pending) 项待处理")
                }
                if active {
                    Image(systemName: "checkmark").font(.caption.weight(.semibold)).foregroundStyle(Color.accentColor)
                }
            }.contentShape(Rectangle())
        }.buttonStyle(.plain)
            .contextMenu {
                Button {
                    Task { await model.toggleMute(session.id) }
                } label: {
                    Label(muted ? "取消静音待办" : "静音待办（仅本机）",
                          systemImage: muted ? "bell" : "bell.slash")
                }
                if client.record.grant.permissions.contains(.sessionRename) {
                    Button {
                        Task { await setPinned(client, session, !session.pinned) }
                    } label: {
                        Label(session.pinned ? "取消置顶" : "置顶",
                              systemImage: session.pinned ? "pin.slash" : "pin")
                    }
                    Button {
                        Task { await setArchived(client, session, !session.archived) }
                    } label: {
                        Label(session.archived ? "取消归档" : "归档",
                              systemImage: session.archived ? "archivebox" : "archivebox.fill")
                    }
                }
                if client.record.grant.permissions.contains(.sessionDelete) {
                    Button(role: .destructive) { deleteTarget = session } label: {
                        Label("永久删除", systemImage: "trash")
                    }
                }
            }
            .swipeActions(edge: .leading, allowsFullSwipe: false) {
                if client.record.grant.permissions.contains(.sessionRename) {
                    Button {
                        Task { await setPinned(client, session, !session.pinned) }
                    } label: {
                        Label(session.pinned ? "取消置顶" : "置顶",
                              systemImage: session.pinned ? "pin.slash" : "pin")
                    }.tint(.blue)
                }
            }
            .swipeActions(edge: .trailing, allowsFullSwipe: false) {
                if client.record.grant.permissions.contains(.sessionDelete) {
                    Button(role: .destructive) { deleteTarget = session } label: {
                        Label("删除", systemImage: "trash")
                    }
                }
                if client.record.grant.permissions.contains(.sessionRename) {
                    Button {
                        Task { await setArchived(client, session, !session.archived) }
                    } label: {
                        Label(session.archived ? "取消归档" : "归档",
                              systemImage: session.archived ? "archivebox" : "archivebox.fill")
                    }.tint(.orange)
                }
            }
            .onAppear {
                guard search.isEmpty, let project,
                      session.id == visible(client, project).last?.id else { return }
                Task { await loadMore(client, project) }
            }
    }

    private func loadMore(_ client: HostClient, _ project: String) async {
        guard client.next[project] != nil, !client.loadingMore.contains(project) else { return }
        client.loadingMore.insert(project)
        defer { client.loadingMore.remove(project) }
        await client.refreshLists(more: project)
    }

    @ViewBuilder private func sidebarActions(_ client: HostClient) -> some View {
        if client.record.grant.permissions.contains(.sessionCreate) {
            Button { creating = true } label: {
                Label("新会话", systemImage: "plus").frame(maxWidth: .infinity, alignment: .leading)
            }.keyboardShortcut("n", modifiers: .command)
                .disabled(!client.ready)
        }
        Button { Task { await client.refreshLists() } } label: {
            Label("刷新会话", systemImage: "arrow.clockwise").frame(maxWidth: .infinity, alignment: .leading)
        }.disabled(!client.ready)
        // Always reachable: without the permission the screen explains which
        // authorization step is missing instead of hiding the feature.
        Button { providersHost = client.record.id } label: {
            Label("服务商与模型", systemImage: "key.horizontal").frame(maxWidth: .infinity, alignment: .leading)
        }
        .disabled(!client.ready)
        .accessibilityIdentifier("providersSettings")
        Button { accountPresented = true } label: {
            Label(model.accountSignedIn ? "中继账号" : "登录中继", systemImage: "person.crop.circle")
                .frame(maxWidth: .infinity, alignment: .leading)
        }.accessibilityIdentifier("hubAccount")
        Button { pairing = true } label: {
            Label("连接电脑", systemImage: "qrcode.viewfinder").frame(maxWidth: .infinity, alignment: .leading)
        }
        Button(role: .destructive) { hostToForget = client.record; forgetConfirmation = true } label: {
            Label("从这台设备移除", systemImage: "trash").frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func pendingOperations(_ client: HostClient) -> [PendingOperation] {
        client.operations.filter { [.prepared, .awaitingConfirmation, .outcomeUnknown].contains($0.status) }
    }

    private func select(_ session: SessionSummary) {
        sessionID = session.id
        withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false }
    }

    /// Opens a Session straight from the inbox: set the target first, then let the
    /// list catch up, because the entry may sit on a page that is not loaded yet.
    private func open(_ entry: InboxEntry, client: HostClient) {
        sessionID = entry.sessionID
        withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false }
        Task { await client.refreshLists() }
    }

    /// Archived Sessions stay out of the way until the owner asks to see them.
    private func visible(_ client: HostClient, _ project: String) -> [SessionSummary] {
        (client.lists[project] ?? []).filter { showArchived || !$0.archived }
    }

    private func setPinned(_ client: HostClient, _ session: SessionSummary, _ pinned: Bool) async {
        _ = await client.session(session).command(.sessionPin, kind: .sessionPin,
                                                  payload: .object(["pinned": .bool(pinned)]))
        await client.refreshLists()
    }

    private func setArchived(_ client: HostClient, _ session: SessionSummary, _ archived: Bool) async {
        _ = await client.session(session).command(.sessionArchive, kind: .sessionArchive,
                                                  payload: .object(["archived": .bool(archived)]))
        await client.refreshLists()
    }

    private var emptySidebar: some View {
        List {
            Section {
                HStack {
                    Text("miao").font(.headline)
                    Spacer()
                    Button { withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false } } label: {
                        Image(systemName: "xmark")
                    }.accessibilityLabel("关闭侧栏").buttonStyle(.plain)
                }
                Button { accountPresented = true } label: {
                    Label(model.accountSignedIn ? "中继账号" : "登录中继", systemImage: "person.crop.circle")
                        .frame(maxWidth: .infinity, alignment: .leading)
                }.accessibilityIdentifier("hubAccount")
                Button { pairing = true } label: {
                    Label("连接电脑", systemImage: "qrcode.viewfinder").frame(maxWidth: .infinity, alignment: .leading)
                }
                if let error = model.error { Text(error).foregroundStyle(.red).font(.footnote) }
                if let error = model.accountError { Text(error).foregroundStyle(.red).font(.footnote) }
            } header: { Text("我的电脑") } footer: {
                Text("在电脑上打开 /remote-control 并登录同一账号，或扫描配对码。")
            }
        }
        .listStyle(.insetGrouped)
        .navigationTitle("miao")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button { withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false } } label: { Image(systemName: "xmark") }
            }
        }
    }

    private func projectName(_ id: String) -> String {
        if id.isEmpty { return "已授权会话" }
        guard let hostID, let client = model.clients[hostID] else { return "项目" }
        return client.projects.first(where: { $0["id"]?.string == id })?["name"]?.string ?? "项目"
    }

    private func applyNotification() {
        guard phase == .active else { return }
        if let target = model.notificationHostID {
            model.notificationHostID = nil
            hostID = target
        }
        if let destination = model.notificationDestination {
            model.notificationDestination = nil
            hostID = destination.hostID
            notificationSession = destination
        }
    }

    /// A notification can arrive before the host registry has loaded. Keep the
    /// target until the matching host appears instead of dropping the request.
    private func consumeDeepLink() {
        guard let target = pendingDeepLink,
              let record = model.hosts.first(where: { $0.host.target.hostID == target.hostID })
        else { return }
        hostID = record.id
        sessionID = target.sessionID
        pendingDeepLink = nil
        withAnimation(.easeInOut(duration: 0.25)) { sidebarVisible = false }
    }

    @MainActor private func attach() async {
        attachEpoch += 1
        let generation = attachEpoch
        let target = hostID
        let previous = attached
        attached = target
        if let previous, previous != target { await model.clients[previous]?.removeScene(sceneID) }
        guard attachEpoch == generation else { return }
        // The environment phase inside a task is the value captured when the
        // task started, which on a cold launch can still be `.inactive` at the
        // moment the async work resumes. Registering that stale value leaves the
        // host at "等待连接" until the next background/foreground cycle, so read
        // the live application state instead.
        let current = liveScenePhase
        model.scene(sceneID, phase: current)
        if let target { await model.clients[target]?.scene(sceneID, phase: current) }
    }

    private var liveScenePhase: ScenePhase {
        switch UIApplication.shared.applicationState {
        case .active: return .active
        case .inactive: return .inactive
        case .background: return .background
        @unknown default: return .background
        }
    }
}

struct DeepLinkTarget: Equatable {
    let hostID: String
    let sessionID: String

    init?(url: URL) {
        guard url.scheme?.lowercased() == "miao", url.host?.lowercased() == "session" else { return nil }
        let parts = url.pathComponents.filter { $0 != "/" }
        guard parts.count >= 2 else { return nil }
        hostID = parts[0]
        sessionID = parts[1]
    }
}

struct ComposerAttachment: Identifiable, Equatable {
    let id: UUID
    let name: String
    let mimeType: String
    let data: Data

    var image: UIImage? { UIImage(data: data) }
}

@MainActor
private struct PairingView: View {
    @Bindable var model: AppModel
    @Binding var initialURI: String
    @Environment(\.dismiss) private var dismiss
    @State private var uri = ""
    @State private var label = UIDevice.current.userInterfaceIdiom == .pad ? "我的 iPad" : "我的 iPhone"
    @State private var scanning = false
    @State private var previousHosts = Set<UUID>()

    var body: some View {
        NavigationStack {
            Form {
                Section("设备名称") { TextField("这台设备的名称", text: $label).textContentType(.nickname) }
                Section {
                    Button("扫描电脑上的二维码", systemImage: "qrcode.viewfinder") { scanning = true }
                    TextField("粘贴 miao 配对链接", text: $uri, axis: .vertical)
                        .lineLimit(1...4)
                        .textInputAutocapitalization(.never).autocorrectionDisabled().privacySensitive()
                        .accessibilityIdentifier("pairingURI")
                    Button("开始配对") { model.pair(uri.trimmingCharacters(in: .whitespacesAndNewlines), label: label); uri = "" }
                        .disabled(model.pairing != nil || uri.isEmpty || label.isEmpty || label.utf16.count > 128)
                } header: { Text("连接电脑") } footer: { Text("链接只用于一次配对。请核对电脑和手机显示的设备指纹，再在电脑上批准。") }
                if let progress = model.pairing {
                    Section { ProgressView(progress); if let fingerprint = model.fingerprint {
                        Text(fingerprint).font(.system(.footnote, design: .monospaced)).textSelection(.enabled)
                    }; Button("取消配对", role: .cancel) { model.cancelPairing() } }
                }
                if let error = model.error { Section { Text(error).foregroundStyle(.red) } }
            }
            .navigationTitle("连接电脑")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("关闭") { model.cancelPairing(); dismiss() } } }
            .onAppear { uri = initialURI; previousHosts = Set(model.hosts.map(\.id)) }
            .onChange(of: model.hosts.map(\.id)) { _, ids in
                if ids.contains(where: { !previousHosts.contains($0) }) { dismiss() }
            }
            .sheet(isPresented: $scanning) {
                ScannerView(scanned: { value in uri = value; scanning = false }, manual: { scanning = false })
            }
        }
    }
}

@MainActor
private struct CreateSessionView: View {
    @Bindable var client: HostClient
    var onCreated: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var creating = false
    var body: some View {
        NavigationStack {
            List {
                ForEach(Array(client.projects.enumerated()), id: \.offset) { _, project in
                    if let projectID = project["id"]?.string {
                        Section(project["name"]?.string ?? "项目") {
                            ForEach(Array((project["directories"]?.array ?? []).enumerated()), id: \.offset) { _, directory in
                                if let directoryID = directory["id"]?.string {
                                    Button(directory["name"]?.string ?? "目录") {
                                        creating = true
                                        Task {
                                            do {
                                                let created = try await client.mutate(.sessionCreate, kind: .sessionCreate,
                                                    target: OperationTarget(hostID: client.record.host.target.hostID,
                                                        runtimeID: client.record.host.target.runtimeID, projectID: projectID),
                                                    payload: .object(["directoryID": .string(directoryID)]))
                                                await client.refreshLists()
                                                if let id = created["data"]?["id"]?.string { onCreated(id) }
                                                dismiss()
                                            } catch { client.error = userMessage(error) }
                                            creating = false
                                        }
                                    }.disabled(creating || !client.ready)
                                }
                            }
                        }
                    }
                }
                if let error = client.error { Text(error).foregroundStyle(.red) }
            }
            .navigationTitle("选择工作目录")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("关闭") { dismiss() } } }
        }
    }
}

@MainActor
private struct SessionView: View {
    @Bindable var client: HostClient
    @Bindable var session: RemoteSession
    var showsSidebarToggle = false
    var onSidebar: (() -> Void)?
    @State private var speech = SpeechInput()
    @State private var pickedPhotos: [PhotosPickerItem] = []
    @State private var attachments: [ComposerAttachment] = []
    @State private var diffPresented = false
    @State private var renamePresented = false
    @State private var selectionPresented = false
    @State private var menuPresented = false
    @State private var selectedAction: String?
    @State private var title = ""
    @State private var queue = false
    @State private var followsLatest = true
    @State private var viewport: CGFloat = 0
    @State private var searchPresented = false
    @State private var jumpTarget: String?
    private let bottomID = "session-bottom"
    /// Queueing is only meaningful while a turn is running.
    private var running: Bool { session.pending["execution"]?["type"]?.string == "running" }
    private var executionID: String? {
        guard running else { return nil }
        return session.pending["execution"]?["executionID"]?.string
    }
    private var waitingCount: Int {
        (session.pending["permissions"]?.array?.count ?? 0)
            + (session.pending["questions"]?.array?.count ?? 0)
    }
    @State private var notice: String?
    @FocusState private var composing: Bool
    @Environment(\.scenePhase) private var phase

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 20) {
                    if session.loading { ProgressView("正在同步会话…") }
                    if session.timeline.revert != nil { Label("电脑上有待确认的回退操作", systemImage: "arrow.uturn.backward").font(.footnote) }
                    ForEach(Array(session.displayed.enumerated()), id: \.element.messageID) { _, message in
                        MessageView(message: message)
                    }
                    ForEach(session.outbox) { item in
                        OutboxView(item: item,
                            retry: { Task { await session.retry(item) } },
                            reconcile: { Task { await session.reconcile(item) } },
                            dismiss: { session.dismissOutbox(item.id) })
                    }
                    PendingView(client: client, session: session).id("pending-requests")
                    // A real anchor at the end scrolls past the last bubble, which
                    // anchoring to a message cannot do while it grows.
                    Color.clear.frame(height: 1).id(bottomID)
                        .background(GeometryReader { geo in
                            Color.clear.onChange(of: geo.frame(in: .named("session")).minY) { _, y in
                                guard viewport > 0 else { return }
                                followsLatest = y <= viewport
                            }
                        })
                }.padding()
            }
            .coordinateSpace(name: "session")
            .scrollDismissesKeyboard(.interactively)
            .refreshable { await session.refresh() }
            .onChange(of: session.revision) { _, _ in pin(proxy) }
            .onAppear { pin(proxy) }
            .onChange(of: jumpTarget) { _, target in
                guard let target else { return }
                jumpTarget = nil
                followsLatest = false
                jump(proxy, to: target)
            }
            .overlay(alignment: .bottomTrailing) {
                if !followsLatest {
                    Button {
                        followsLatest = true
                        pin(proxy)
                    } label: {
                        Label("跳到最新", systemImage: "arrow.down")
                            .font(.footnote).padding(.horizontal, 10).padding(.vertical, 6)
                            .background(.thinMaterial, in: Capsule())
                    }
                    .padding(.trailing, 12).padding(.bottom, 12)
                    .accessibilityIdentifier("jumpLatest")
                }
            }
            .background(GeometryReader { geo in
                Color.clear.onAppear { viewport = geo.size.height }
                    .onChange(of: geo.size.height) { _, value in viewport = value }
            })
            .safeAreaInset(edge: .top) { sessionBanner(proxy) }
        }
        .safeAreaInset(edge: .bottom) { composer }
        .navigationTitle(session.timeline.title.isEmpty ? session.summary.title : session.timeline.title)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if showsSidebarToggle, let onSidebar {
                ToolbarItem(placement: .topBarLeading) {
                    Button(action: onSidebar) { Image(systemName: "line.3.horizontal") }
                        .accessibilityLabel("会话侧栏").accessibilityIdentifier("sessionSidebar")
                }
            }
            if let executionID {
                ToolbarItem(placement: .topBarTrailing) {
                    Button(role: .destructive) {
                        UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                        Task { _ = await session.command(.sessionInterrupt, kind: .interrupt,
                            payload: .object(["executionID": .string(executionID)])) }
                    } label: { Image(systemName: "stop.circle.fill") }
                        .accessibilityLabel("停止执行")
                        .disabled(!client.ready || client.sending(session.summary.id))
                }
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button { searchPresented = true } label: { Image(systemName: "magnifyingglass") }
                    .accessibilityLabel("搜索消息")
                    .keyboardShortcut("k", modifiers: .command)
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button { composing = false; menuPresented = true } label: { Image(systemName: "ellipsis.circle") }.accessibilityLabel("会话菜单")
                    .accessibilityIdentifier("sessionMenu").accessibilityValue(client.connectionDescription).disabled(!client.ready)
            }
        }
        .toolbar {
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button("完成") { composing = false }.accessibilityIdentifier("dismissKeyboard")
            }
        }
        .sheet(isPresented: $menuPresented, onDismiss: {
            let action = selectedAction
            selectedAction = nil
            if action == "rename" {
                title = session.timeline.title.isEmpty ? session.summary.title : session.timeline.title
                renamePresented = true
            }
            if action == "selection" { selectionPresented = true }
            if action == "search" { searchPresented = true }
            if action == "diff" { Task { await session.loadDiff(); diffPresented = session.diff != nil } }
        }) {
            NavigationStack {
                Form {
                    if client.record.grant.permissions.contains(.sessionSelection) {
                        Button("Agent / 模型") { selectedAction = "selection"; menuPresented = false }
                    }
                    Button("搜索消息") { selectedAction = "search"; menuPresented = false }
                        .accessibilityIdentifier("searchMessages")
                    Button("查看文件变化") { selectedAction = "diff"; menuPresented = false }
                    if client.record.grant.permissions.contains(.sessionRename) {
                        Button("重命名") { selectedAction = "rename"; menuPresented = false }
                    }
                    ShareLink(item: transcriptText) {
                        Label("导出对话", systemImage: "square.and.arrow.up")
                    }
                    .disabled(session.displayed.isEmpty)
                    if let share = session.summary.share {
                        ShareLink(item: share) {
                            Label("分享会话链接", systemImage: "link")
                        }
                        Button("复制会话链接") {
                            UIPasteboard.general.string = share
                        }
                    }
                    Button("复制最后回复") {
                        if let text = lastAssistantText { UIPasteboard.general.string = text }
                    }
                    .disabled(lastAssistantText == nil)
                }
                .navigationTitle("会话操作")
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("关闭") { menuPresented = false } } }
            }
            .presentationDetents([.medium])
        }
        .onAppear { session.appear() }
        .onDisappear { speech.stop(); session.disappear() }
        .onChange(of: phase) { _, value in if value == .background { speech.stop(); composing = false } }
        .sheet(isPresented: $diffPresented) {
            NavigationStack {
                ScrollView { if let diff = session.diff { DiffView(diff: diff) } }
                    .navigationTitle("文件变化")
                    .toolbar { Button("关闭") { diffPresented = false } }
            }
        }
        .sheet(isPresented: $searchPresented) {
            MessageSearchView(messages: session.displayed) { jumpTarget = $0 }
        }
        .sheet(isPresented: $selectionPresented) {
            SessionSelectionView(client: client, session: session)
        }
        .sheet(isPresented: $renamePresented) {
            RenameSessionView(initialTitle: title) { value in
                Task { _ = await session.command(.sessionRename, kind: .sessionRename, payload: .object(["title": .string(value)])) }
            }
        }
    }

    /// A banner that survives scrolling. Connection loss, a failed command and
    /// a waiting approval all otherwise live at the bottom of a long transcript.
    @ViewBuilder private func sessionBanner(_ proxy: ScrollViewProxy) -> some View {
        if !client.ready {
            bannerRow(icon: "wifi.slash", text: client.connectionDescription, tint: .orange)
        } else if let error = session.error ?? client.error {
            bannerRow(icon: "exclamationmark.triangle.fill", text: error, tint: .red, action: "重试") {
                Task { await session.refresh() }
            }
        } else if waitingCount > 0 {
            bannerRow(icon: "hand.raised.fill", text: "\(waitingCount) 项等待你处理", tint: .orange, action: "查看") {
                followsLatest = false
                jump(proxy, to: "pending-requests")
            }
        }
    }

    private func bannerRow(icon: String, text: String, tint: Color,
                           action: String? = nil, perform: (() -> Void)? = nil) -> some View {
        HStack(spacing: 8) {
            Image(systemName: icon)
            Text(text).font(.footnote).lineLimit(2)
            Spacer(minLength: 4)
            if let action, let perform {
                Button(action, action: perform).font(.footnote.weight(.semibold))
            }
        }
        .foregroundStyle(tint)
        .padding(.horizontal, 12).padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.12))
    }

    private func pin(_ proxy: ScrollViewProxy) {
        guard followsLatest else { return }
        proxy.scrollTo(bottomID, anchor: .bottom)
    }

    /// LazyVStack renders on demand: a second pass settles rows the first pass
    /// could not have materialized yet.
    private func jump(_ proxy: ScrollViewProxy, to id: String) {
        proxy.scrollTo(id, anchor: .top)
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(120))
            proxy.scrollTo(id, anchor: .top)
        }
    }

    private var transcriptText: String {
        session.displayed.map { message in
            let role = switch message["type"]?.string {
            case "user": "我"
            case "assistant": "miao"
            default: message["type"]?.string ?? "记录"
            }
            return "**\(role)**\n\n" + readableText(message)
        }
        .filter { !$0.hasSuffix("\n\n") }
        .joined(separator: "\n\n---\n\n")
    }

    private var lastAssistantText: String? {
        session.displayed.last(where: { $0["type"]?.string == "assistant" })
            .map(readableText)
            .flatMap { $0.isEmpty ? nil : $0 }
    }

    private func readableText(_ message: JSONValue) -> String {
        let parts = [message["text"]?.string, message["summary"]?.string,
                     message["command"]?.string, message["output"]?.string]
            + (message["content"]?.array ?? []).compactMap { part in
                part["type"]?.string == "text" ? part["text"]?.string : nil
            }
        return parts.compactMap { $0 }.joined(separator: "\n")
    }

    private var composer: some View {
        VStack(spacing: 8) {
            if speech.active {
                VoiceInputPanel(speech: speech, tint: .accentColor,
                                onCancel: cancelDictation, onFinish: finishDictation)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
            VoiceInputNotice(speech: speech)
            if let notice {
                Text(notice).font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("composerNotice")
            }
            if !session.queuedPrompts.isEmpty {
                QueuedPromptsView(prompts: session.queuedPrompts)
            }
            if client.record.grant.permissions.contains(.prompt) {
                if !attachments.isEmpty {
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 8) {
                            ForEach(attachments) { item in
                                ZStack(alignment: .topTrailing) {
                                    if let image = item.image {
                                        Image(uiImage: image)
                                            .resizable().scaledToFill()
                                            .frame(width: 56, height: 56)
                                            .clipShape(RoundedRectangle(cornerRadius: 8))
                                    }
                                    Button {
                                        attachments.removeAll { $0.id == item.id }
                                    } label: {
                                        Image(systemName: "xmark.circle.fill")
                                            .foregroundStyle(.white, .black.opacity(0.55))
                                    }
                                    .padding(2)
                                    .accessibilityLabel("移除附件 \(item.name)")
                                }
                            }
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                TextField("给 miao 的消息", text: Binding(get: { session.draft }, set: { session.edit($0) }), axis: .vertical)
                    .focused($composing)
                    .lineLimit(2...8).padding(10).background(.background, in: RoundedRectangle(cornerRadius: 12))
                    .disabled(speech.active).accessibilityIdentifier("messageDraft")
                HStack {
                    if !speech.active {
                        SpeechMicButton(speech: speech, identifier: "remoteDictate", action: startDictation)
                        SpeechLanguageMenu(speech: speech)
                    }
                    if client.record.grant.permissions.contains(.sessionSelection) {
                        Button { selectionPresented = true } label: {
                            Image(systemName: "cpu")
                        }
                        .accessibilityLabel("切换 Agent 或模型")
                    }
                    if client.capabilities.supportsAttachments, !speech.active {
                        PhotosPicker(selection: $pickedPhotos, maxSelectionCount: 4, matching: .images) {
                            Image(systemName: "photo.on.rectangle")
                        }
                        .accessibilityLabel("添加图片")
                        .accessibilityIdentifier("attachImage")
                    }
                    if !client.capabilities.commands.isEmpty, !speech.active {
                        Menu {
                            ForEach(client.capabilities.commands) { command in
                                Button {
                                    let prefix = session.draft.isEmpty || session.draft.hasSuffix(" ") ? "" : " "
                                    session.edit(session.draft + prefix + "/" + command.name + " ")
                                } label: {
                                    Text("/" + command.name + (command.description.isEmpty ? "" : " · " + command.description))
                                }
                            }
                        } label: {
                            Image(systemName: "slash.circle")
                        }
                        .accessibilityLabel("插入命令")
                    }
                    Spacer()
                    if running {
                        Toggle("排队", isOn: $queue).toggleStyle(.button).font(.caption)
                            .accessibilityIdentifier("queueInput")
                    }
                    Button("发送", systemImage: "arrow.up") {
                        UIImpactFeedbackGenerator(style: .light).impactOccurred()
                        speech.stop()
                        // Queueing is a per-message choice, not a mode: a sticky
                        // toggle silently queued every later message.
                        let queued = running && queue
                        queue = false
                        notice = queued ? "已排队：这一轮结束后发送" : nil
                        let sendingAttachments = attachments
                        attachments = []
                        Task {
                            await session.send(queue: queued, attachments: sendingAttachments)
                            if queued {
                                try? await Task.sleep(for: .seconds(3))
                                if notice?.hasPrefix("已排队") == true { notice = nil }
                            }
                        }
                    }
                        .buttonStyle(.borderedProminent)
                        .disabled(!client.ready || client.sending(session.summary.id)
                            || (session.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && attachments.isEmpty)
                            || session.draft.utf16.count > (client.capabilities.maxPromptCharacters ?? 65_536))
                        .keyboardShortcut(.return, modifiers: .command)
                        .accessibilityIdentifier("sendMessage")
                }
            } else { Text("此设备拥有只读会话权限").font(.footnote).foregroundStyle(.secondary) }
        }.padding().background(.bar)
            .animation(.snappy(duration: 0.22), value: speech.active)
            .onChange(of: pickedPhotos) { _, items in
                guard !items.isEmpty else { return }
                Task { await loadPhotos(items) }
            }
    }

    private func startDictation() {
        composing = false
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        Task { await speech.start(text: session.draft, locale: speech.localeIdentifier) { expected, value in
            guard session.draft == expected else { return false }
            session.edit(value)
            return true
        } }
    }

    private func finishDictation() {
        speech.stop()
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        UIAccessibility.post(notification: .announcement, argument: "听写已结束，文字保留在输入框")
    }

    private func cancelDictation() {
        speech.cancel { session.edit($0) }
        UIAccessibility.post(notification: .announcement, argument: "已取消听写，恢复原草稿")
    }

    private func loadPhotos(_ items: [PhotosPickerItem]) async {
        var next = attachments
        for item in items.prefix(max(0, 4 - next.count)) {
            guard let data = try? await item.loadTransferable(type: Data.self),
                  let image = UIImage(data: data),
                  let jpeg = downscaledJPEG(image) else { continue }
            next.append(ComposerAttachment(id: UUID(), name: "图片 \(next.count + 1).jpg",
                                           mimeType: "image/jpeg", data: jpeg))
        }
        attachments = next
        pickedPhotos = []
    }

    private func downscaledJPEG(_ image: UIImage) -> Data? {
        let maximum: CGFloat = 1600
        let scale = min(1, maximum / max(image.size.width, image.size.height))
        let size = CGSize(width: image.size.width * scale, height: image.size.height * scale)
        let resized = UIGraphicsImageRenderer(size: size).image { _ in
            image.draw(in: CGRect(origin: .zero, size: size))
        }
        return resized.jpegData(compressionQuality: 0.8)
    }
}

private extension JSONValue { var messageID: String { self["id"]?.string ?? "" } }

/// A prompt whose admission is still being confirmed. It mirrors the user
/// bubble so the phone never looks like it dropped the message while waiting.
@MainActor
private struct OutboxView: View {
    let item: OutboxPrompt
    let retry: () -> Void
    let reconcile: () -> Void
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .bottom) {
            Spacer(minLength: 56)
            VStack(alignment: .trailing, spacing: 4) {
                Text(item.text)
                    .padding(12)
                    .background(Color.accentColor.opacity(0.14), in: RoundedRectangle(cornerRadius: 16))
                HStack(spacing: 8) {
                    switch item.status {
                    case .sending:
                        ProgressView().controlSize(.mini)
                        Text(item.queued ? "排队中…" : "发送中…")
                    case .failed:
                        Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.red)
                        Text("发送失败").foregroundStyle(.red)
                        if item.retryable { Button("重试", action: retry) }
                        Button("删除", role: .destructive, action: dismiss)
                    case .uncertain:
                        Image(systemName: "questionmark.circle.fill").foregroundStyle(.orange)
                        Text("结果待核对").foregroundStyle(.orange)
                        Button("核对", action: reconcile)
                        Button("删除", role: .destructive, action: dismiss)
                    }
                }
                .font(.caption)
                if let detail = item.detail {
                    Text(detail).font(.caption2).foregroundStyle(.secondary)
                        .lineLimit(2).multilineTextAlignment(.trailing)
                }
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("outbox-\(item.id.uuidString)")
    }
}

@MainActor
private struct MessageView: View, Equatable {
    let message: JSONValue

    nonisolated static func == (lhs: Self, rhs: Self) -> Bool { lhs.message == rhs.message }

    var body: some View {
        Group {
            if message["type"]?.string == "user" {
                HStack(alignment: .bottom) {
                    Spacer(minLength: 56)
                    content
                        .padding(12)
                        .background(Color.accentColor.opacity(0.14), in: RoundedRectangle(cornerRadius: 16))
                }
            } else {
                content
                    .padding(.vertical, 2)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .contextMenu {
            Button { UIPasteboard.general.string = plainText } label: {
                Label("复制", systemImage: "doc.on.doc")
            }
            .disabled(plainText.isEmpty)
        }
    }

    private var plainText: String {
        let parts = [message["text"]?.string, message["summary"]?.string, message["command"]?.string,
                     message["output"]?.string]
            + (message["content"]?.array ?? []).compactMap { $0["text"]?.string }
        return parts.compactMap { $0 }.joined(separator: "\n")
    }

    private var content: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let attachments = message["attachments"]?.array, !attachments.isEmpty {
                AttachmentStrip(attachments: attachments)
            }
            if let text = message["text"]?.string { Text(text).textSelection(.enabled) }
            if let summary = message["summary"]?.string { LocalMarkdown(text: summary) }
            ForEach(Array((message["content"]?.array ?? []).enumerated()), id: \.offset) { _, part in
                if part["type"]?.string == "reasoning" {
                    DisclosureGroup("思考过程") {
                        ExpandingText(text: part["text"]?.string ?? part.formatted, font: .footnote)
                            .foregroundStyle(.secondary)
                    }
                } else if part["type"]?.string == "text", let text = part["text"]?.string {
                    LocalMarkdown(text: text)
                } else if let text = part["text"]?.string {
                    Text(text).textSelection(.enabled)
                } else {
                    ToolPartView(part: part)
                }
            }
            if let command = message["command"]?.string {
                DisclosureGroup { ExpandingText(text: command) } label: {
                    Label("命令", systemImage: "terminal").font(.footnote.weight(.medium))
                }.accentColor(.secondary)
            }
            if let output = message["output"]?.string {
                DisclosureGroup { ExpandingText(text: output) } label: {
                    Label("输出", systemImage: "text.quote").font(.footnote.weight(.medium))
                }.accentColor(.secondary)
            }
            if ["agent-switched", "model-switched"].contains(message["type"]?.string ?? "") {
                Text(message["agent"]?.string ?? message["model"]?.formatted ?? "").font(.footnote).foregroundStyle(.secondary).textSelection(.enabled)
            }
            if message["type"]?.string == "assistant" { AssistantMetrics(message: message) }
            if let error = message["error"] { Text(error.formatted).font(.footnote).foregroundStyle(.red) }
        }
    }
}

/// A tool call is context, not content: one line with its live status and
/// duration, expandable for the full input/result.
@MainActor
private struct AttachmentStrip: View {
    let attachments: [JSONValue]

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(Array(attachments.enumerated()), id: \.offset) { _, attachment in
                    if let encoded = attachment["data"]?.string,
                       let data = Data(base64Encoded: encoded),
                       let image = UIImage(data: data) {
                        Image(uiImage: image)
                            .resizable().scaledToFill()
                            .frame(width: 96, height: 96)
                            .clipShape(RoundedRectangle(cornerRadius: 10))
                            .accessibilityLabel(attachment["name"]?.string ?? "图片")
                    } else if let name = attachment["name"]?.string {
                        Label(name, systemImage: "doc")
                            .font(.footnote)
                    }
                }
            }
        }
    }
}

@MainActor
private struct ToolPartView: View {
    let part: JSONValue
    @State private var expanded = false

    private var state: JSONValue? { part["state"] }
    private var status: String { state?["status"]?.string ?? "pending" }
    private var name: String { part["name"]?.string ?? part["type"]?.string ?? "工具" }
    private var tint: Color {
        switch status {
        case "running": return .blue
        case "error": return .red
        case "completed": return .green
        default: return .secondary
        }
    }
    private var icon: String {
        switch status {
        case "running": return "arrow.triangle.2.circlepath"
        case "error": return "xmark.octagon.fill"
        case "completed": return "checkmark.circle.fill"
        default: return "clock"
        }
    }
    private var duration: String? {
        guard let ran = part["time"]?["ran"]?.number,
              let completed = part["time"]?["completed"]?.number,
              completed >= ran else { return nil }
        let milliseconds = completed - ran
        return milliseconds >= 1000
            ? String(format: "%.1fs", milliseconds / 1000)
            : String(format: "%.0fms", milliseconds)
    }
    private var detail: String {
        if let error = state?["error"]?.formatted, !error.isEmpty { return error }
        if let content = state?["content"]?.array, !content.isEmpty {
            return content.map(\.formatted).joined(separator: "\n")
        }
        return part.formatted
    }

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            ExpandingText(text: detail)
        } label: {
            HStack(spacing: 6) {
                Image(systemName: icon).foregroundStyle(tint)
                Text(name).lineLimit(1)
                if let duration { Text(duration).foregroundStyle(.secondary) }
                Spacer(minLength: 0)
                if status == "error" { Text("失败").foregroundStyle(.red) }
            }
            .font(.footnote.weight(.medium))
        }
        .accentColor(.secondary)
        .padding(6)
        .background(tint.opacity(status == "error" ? 0.08 : 0), in: RoundedRectangle(cornerRadius: 8))
    }
}

@MainActor
private struct AssistantMetrics: View {
    let message: JSONValue

    private var tokens: JSONValue? { message["tokens"] }
    private var total: Int {
        ["input", "output", "reasoning"].reduce(0) { $0 + Int(tokens?[$1]?.number ?? 0) }
    }
    private var cost: Double? { message["cost"]?.number }
    private var ttft: Double? { message["ttft"]?.number }

    var body: some View {
        if total > 0 || cost != nil || ttft != nil {
            HStack(spacing: 10) {
                if let cost { Text(String(format: "$%.3f", cost)) }
                if total > 0 { Text(compact(total) + " tokens") }
                if let ttft, ttft > 0 { Text(String(format: "首字 %.1fs", ttft / 1000)) }
            }
            .font(.caption2)
            .foregroundStyle(.secondary)
            .accessibilityElement(children: .combine)
            .accessibilityLabel("用量：\(accessibilitySummary)")
        }
    }

    private func compact(_ value: Int) -> String {
        value >= 1000 ? String(format: "%.1fk", Double(value) / 1000) : "\(value)"
    }

    private var accessibilitySummary: String {
        var parts: [String] = []
        if let cost { parts.append(String(format: "成本 %.3f 美元", cost)) }
        if total > 0 { parts.append("\(total) 个 token") }
        if let ttft, ttft > 0 { parts.append(String(format: "首字 %.1f 秒", ttft / 1000)) }
        return parts.joined(separator: "，")
    }
}

/// Caps oversized tool output in the view: collapsed by default with an expand
/// action, so one huge read or shell result cannot stall layout.
@MainActor
private struct ExpandingText: View {
    let text: String
    var font: Font = .system(.footnote, design: .monospaced)
    @State private var expanded = false

    private static let limit = 20_000

    var body: some View {
        let truncated = !expanded && text.count > Self.limit
        VStack(alignment: .leading, spacing: 6) {
            Text(truncated ? String(text.prefix(Self.limit)) + "\n…" : text)
                .font(font).textSelection(.enabled)
            if truncated {
                Button("展开全部（\(text.count) 字符）") { expanded = true }
                    .font(.footnote)
            }
        }
    }
}

/// Renders the structured file diff line by line instead of one monospaced
/// Text, so a large change set stays scrollable.
@MainActor
private struct DiffView: View {
    let diff: JSONValue
    @State private var expanded: Set<Int> = []

    private struct FileChange: Identifiable {
        let id: Int
        let name: String
        let patch: String
        let additions: Int
        let deletions: Int
    }

    private static let lineLimit = 2000

    private var files: [FileChange] {
        (diff["data"]?.array ?? []).enumerated().map { index, value in
            let patch = value["patch"]?.string ?? ""
            let lines = patch.split(separator: "\n", omittingEmptySubsequences: false)
            return FileChange(
                id: index,
                name: value["file"]?.string ?? "文件 \(index + 1)",
                patch: patch,
                additions: lines.filter { $0.hasPrefix("+") && !$0.hasPrefix("+++") }.count,
                deletions: lines.filter { $0.hasPrefix("-") && !$0.hasPrefix("---") }.count
            )
        }
    }

    var body: some View {
        LazyVStack(alignment: .leading, spacing: 10) {
            if files.isEmpty {
                Text("没有文件变化").font(.footnote).foregroundStyle(.secondary)
            }
            ForEach(files) { file in
                DisclosureGroup(isExpanded: binding(file.id)) {
                    let lines = Array(file.patch.split(separator: "\n", omittingEmptySubsequences: false)
                        .prefix(Self.lineLimit))
                    if lines.isEmpty {
                        Text("无文本差异").font(.footnote).foregroundStyle(.secondary)
                    }
                    ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                        Text(String(line))
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundStyle(color(line) ?? .primary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .textSelection(.enabled)
                    }
                    if file.patch.split(separator: "\n", omittingEmptySubsequences: false).count > Self.lineLimit {
                        Text("仅显示前 \(Self.lineLimit) 行").font(.caption2).foregroundStyle(.secondary)
                    }
                } label: {
                    VStack(alignment: .leading, spacing: 3) {
                        Text(file.name)
                            .font(.system(.footnote, design: .monospaced))
                            .lineLimit(2)
                        HStack(spacing: 8) {
                            Text("+\(file.additions)").foregroundStyle(.green)
                            Text("-\(file.deletions)").foregroundStyle(.red)
                            Spacer()
                            Text("\(file.additions + file.deletions) 行变化").foregroundStyle(.secondary)
                        }
                        .font(.caption2)
                    }
                }
            }
        }
        .padding()
    }

    private func binding(_ id: Int) -> Binding<Bool> {
        Binding(
            get: { expanded.contains(id) },
            set: { open in
                if open { expanded.insert(id) } else { expanded.remove(id) }
            }
        )
    }

    private func color(_ line: Substring) -> Color? {
        if line.hasPrefix("+++") || line.hasPrefix("---") { return .secondary }
        if line.hasPrefix("@@") { return .blue }
        if line.hasPrefix("+") { return .green }
        if line.hasPrefix("-") { return .red }
        return nil
    }
}

/// Searches the loaded timeline and jumps to the tapped message. The scan runs
/// off the main actor: a 10k-message transcript must not hitch the sheet.
@MainActor
private struct MessageSearchView: View {
    let messages: [JSONValue]
    let onSelect: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var query = ""
    @State private var matches: [Match] = []
    @State private var searching = false

    private struct Match: Identifiable, Sendable {
        let id: String
        let snippet: String
    }

    nonisolated private static let resultLimit = 200

    var body: some View {
        NavigationStack {
            List {
                Section {
                    ForEach(matches) { match in
                        Button {
                            onSelect(match.id)
                            dismiss()
                        } label: {
                            Text(match.snippet).font(.footnote).lineLimit(3)
                        }.buttonStyle(.plain)
                    }
                } header: {
                    if searching {
                        ProgressView("正在搜索…")
                    } else if query.count >= 2 {
                        Text(matches.count >= Self.resultLimit
                             ? "前 \(Self.resultLimit) 条结果"
                             : "\(matches.count) 条结果")
                    }
                }
            }
            .overlay {
                if !searching, !query.isEmpty, matches.isEmpty {
                    ContentUnavailableView.search(text: query)
                }
            }
            .navigationTitle("搜索消息")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("关闭") { dismiss() } } }
            .searchable(text: $query, prompt: "在会话中搜索")
            .task(id: query) {
                let term = query
                guard term.count >= 2 else {
                    matches = []
                    searching = false
                    return
                }
                searching = true
                try? await Task.sleep(for: .milliseconds(250))
                guard !Task.isCancelled else { return }
                let found = await Task.detached(priority: .userInitiated) {
                    Self.find(term, in: messages)
                }.value
                guard !Task.isCancelled else { return }
                matches = found
                searching = false
            }
        }
    }

    nonisolated private static func find(_ query: String, in messages: [JSONValue]) -> [Match] {
        var found: [Match] = []
        for message in messages {
            guard let id = message["id"]?.string, !id.isEmpty,
                  let text = searchableText(message),
                  let range = text.range(of: query, options: [.caseInsensitive, .diacriticInsensitive])
            else { continue }
            let start = text.index(range.lowerBound, offsetBy: -40, limitedBy: text.startIndex) ?? text.startIndex
            let end = text.index(range.upperBound, offsetBy: 80, limitedBy: text.endIndex) ?? text.endIndex
            found.append(Match(id: id, snippet: String(text[start..<end])))
            if found.count >= resultLimit { break }
        }
        return found
    }

    nonisolated private static func searchableText(_ message: JSONValue) -> String? {
        let parts = [message["text"]?.string, message["summary"]?.string, message["output"]?.string, message["command"]?.string]
            + (message["content"]?.array ?? []).compactMap { $0["text"]?.string }
        let joined = parts.compactMap { $0 }.joined(separator: "\n")
        return joined.isEmpty ? nil : joined
    }
}

@MainActor
private struct PendingView: View {
    @Bindable var client: HostClient
    @Bindable var session: RemoteSession
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let executionID = session.pending["execution"]?["executionID"]?.string,
               session.pending["execution"]?["type"]?.string == "running" {
                HStack { ProgressView(); Text("电脑正在执行"); Spacer()
                    if client.record.grant.permissions.contains(.interrupt) {
                        Button("停止", role: .destructive) { Task { _ = await session.command(.sessionInterrupt, kind: .interrupt, payload: .object(["executionID": .string(executionID)])) } }
                            .disabled(!client.ready || client.sending(session.summary.id))
                    }
                }
            }
            ForEach(Array((session.pending["permissions"]?.array ?? []).enumerated()), id: \.offset) { _, request in
                if let id = request["id"]?.string {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(request["action"]?.string ?? "权限请求").font(.headline)
                        Text((request["resources"]?.array ?? []).compactMap(\.string).joined(separator: "\n")).font(.footnote).textSelection(.enabled)
                        if client.record.grant.permissions.contains(.permissionReply) {
                            HStack { reply("允许一次", id: id, value: "once"); reply("拒绝", id: id, value: "reject") }
                                .disabled(!client.ready || client.sending(session.summary.id))
                        }
                    }.padding().background(.thinMaterial, in: RoundedRectangle(cornerRadius: 12))
                }
            }
            ForEach(Array((session.pending["questions"]?.array ?? []).enumerated()), id: \.offset) { _, request in
                if let id = request["id"]?.string { QuestionView(client: client, session: session, request: request).id(id) }
            }
            if let data = session.pending["inputs"]?["data"]?.array, !data.isEmpty {
                DisclosureGroup("待处理输入（\(data.count)）") {
                    ForEach(Array(data.enumerated()), id: \.offset) { _, input in Text(input["prompt"]?["text"]?.string ?? input.formatted).font(.footnote) }
                }
            }
        }
    }
    private func reply(_ title: String, id: String, value: String) -> some View {
        Button(title) { Task { _ = await session.command(.permissionReply, kind: .permissionReply, payload: .object(["requestID": .string(id), "reply": .string(value)])) } }.buttonStyle(.bordered)
    }
}

@MainActor
private struct QuestionView: View {
    @Bindable var client: HostClient
    @Bindable var session: RemoteSession
    let request: JSONValue
    @State private var selected: [Int: Set<String>] = [:]
    @State private var custom: [Int: String] = [:]
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array((request["questions"]?.array ?? []).enumerated()), id: \.offset) { index, question in
                Text(question["question"]?.string ?? "问题").font(.headline)
                ForEach(Array((question["options"]?.array ?? []).enumerated()), id: \.offset) { _, option in
                    if let label = option["label"]?.string {
                        Button {
                            var values = selected[index] ?? []
                            if values.contains(label) { values.remove(label) }
                            else if question["multiSelect"]?.bool == true { values.insert(label) }
                            else { values = [label] }
                            selected[index] = values
                        } label: {
                            HStack(alignment: .top) { Image(systemName: selected[index]?.contains(label) == true ? "checkmark.circle.fill" : "circle")
                                VStack(alignment: .leading) { Text(label); if let description = option["description"]?.string { Text(description).font(.caption).foregroundStyle(.secondary) } }
                            }
                        }.buttonStyle(.plain)
                    }
                }
                if question["custom"]?.bool != false {
                    TextField("自定义回答", text: Binding(get: { custom[index] ?? "" }, set: { custom[index] = $0 }))
                }
            }
            if client.record.grant.permissions.contains(.questionReply), let id = request["id"]?.string {
                HStack {
                    Button("提交回答") {
                        let answers = (request["questions"]?.array ?? []).enumerated().map { index, question -> JSONValue in
                            let labels = (question["options"]?.array ?? []).compactMap { $0["label"]?.string }.filter { selected[index]?.contains($0) == true }
                            let text = (custom[index] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
                            let values = text.isEmpty ? labels : question["multiSelect"]?.bool == true ? labels + [text] : [text]
                            return .array(values.map(JSONValue.string))
                        }
                        Task { _ = await session.command(.questionReply, kind: .questionReply, payload: .object(["requestID": .string(id), "answers": .array(answers)])) }
                    }.disabled(!answered)
                    Button("跳过", role: .cancel) { Task { _ = await session.command(.questionReply, kind: .questionReply, payload: .object(["requestID": .string(id), "reject": .bool(true)])) } }
                }.buttonStyle(.bordered).disabled(!client.ready || client.sending(session.summary.id))
            }
        }.padding().background(.thinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }
    private var answered: Bool {
        let questions = request["questions"]?.array ?? []
        return !questions.isEmpty && questions.indices.allSatisfy { !(selected[$0] ?? []).isEmpty || !(custom[$0] ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    }
}

@MainActor
private struct RenameSessionView: View {
    @State private var value: String
    @Environment(\.dismiss) private var dismiss
    let save: (String) -> Void

    init(initialTitle: String, save: @escaping (String) -> Void) {
        _value = State(initialValue: initialTitle)
        self.save = save
    }

    var body: some View {
        NavigationStack {
            Form {
                TextField("会话名称", text: $value).accessibilityIdentifier("renameTitle")
            }
            .navigationTitle("重命名会话")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("取消") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("保存") { save(value); dismiss() }
                        .disabled(value.isEmpty || value.utf16.count > 256)
                }
            }
        }.presentationDetents([.medium])
    }
}

@MainActor
private struct SessionSelectionView: View {
    let client: HostClient
    let session: RemoteSession
    @Environment(\.dismiss) private var dismiss
    @State private var choices: Choices?
    @State private var agent = ""
    @State private var modelIndex = -1
    @State private var variant = ""
    @State private var busy = false
    @State private var failure: String?

    private struct Choices: Decodable {
        struct Agent: Decodable { let id: String }
        struct Model: Decodable {
            struct Variant: Decodable { let id: String }
            let id: String
            let providerID: String
            let name: String
            let variants: [Variant]
        }
        let agents: [Agent]
        let models: [Model]
    }

    private var model: Choices.Model? {
        guard let choices, choices.models.indices.contains(modelIndex) else { return nil }
        return choices.models[modelIndex]
    }

    var body: some View {
        NavigationStack {
            Form {
                Text("保存后从下一次模型调用起生效。")
                if let failure { Text(failure).foregroundStyle(.red) }
                if let choices {
                    Section("Agent") {
                        Picker("Agent", selection: $agent) {
                            Text("请选择").tag("")
                            ForEach(choices.agents, id: \.id) { Text($0.id).tag($0.id) }
                        }.pickerStyle(.navigationLink)
                            .accessibilityIdentifier("agentChoice").accessibilityValue(agent)
                        Button("保存 Agent") {
                            let value = agent
                            busy = true
                            Task {
                                let applied = await session.command(.sessionSwitchAgent, kind: .sessionSwitchAgent, payload: .object(["agent": .string(value)]))
                                busy = false
                                if applied { dismiss() } else { failure = session.error }
                            }
                        }.disabled(agent.isEmpty || busy || !client.ready)
                    }
                    Section("模型") {
                        Picker("模型", selection: $modelIndex) {
                            Text("请选择").tag(-1)
                            ForEach(choices.models.indices, id: \.self) { index in
                                Text(choices.models[index].providerID + " / " + choices.models[index].name).tag(index)
                            }
                        }.pickerStyle(.navigationLink).accessibilityIdentifier("modelChoice")
                            .accessibilityValue(model.map { $0.providerID + " / " + $0.name } ?? "请选择")
                        if let model {
                            Picker("变体", selection: $variant) {
                                Text("默认").tag("")
                                ForEach(model.variants, id: \.id) { Text($0.id).tag($0.id) }
                            }.pickerStyle(.navigationLink)
                                .accessibilityIdentifier("variantChoice").accessibilityValue(variant.isEmpty ? "默认" : variant)
                        }
                        Button("保存模型") {
                            guard let model else { return }
                            let fields: [String: JSONValue] = ["id": .string(model.id), "providerID": .string(model.providerID)]
                                .merging(variant.isEmpty ? [:] : ["variant": .string(variant)]) { _, new in new }
                            busy = true
                            Task {
                                let applied = await session.command(.sessionSwitchModel, kind: .sessionSwitchModel, payload: .object(["model": .object(fields)]))
                                busy = false
                                if applied { dismiss() } else { failure = session.error }
                            }
                        }.disabled(model == nil || busy || !client.ready)
                    }
                } else if failure == nil { ProgressView("正在读取会话设置…") }
            }
            .disabled(busy)
            .navigationTitle("会话设置")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("关闭") { dismiss() }.disabled(busy) } }
        }
        .onChange(of: modelIndex) { _, _ in variant = "" }
        .task {
            do {
                let value = try await client.request(.selectionList, sessionID: session.summary.id)
                try Task.checkCancellation()
                let decoded = try JSONDecoder().decode(Choices.self, from: JSONEncoder().encode(value))
                guard decoded.agents.count <= 256, decoded.models.count <= 4096,
                      decoded.models.allSatisfy({ $0.variants.count <= 256 }) else { throw RemoteRPCError.malformed }
                choices = decoded
            } catch is CancellationError {} catch { failure = userMessage(error) }
        }
    }
}

/// The admitted-but-not-yet-promoted prompts of this Session. The interaction
/// comes from dsh-go: a visible queue instead of a toast that vanishes, so the
/// operator can see what will still fire. Promotion — the transcript carrying
/// the prompt as a user message — removes the row.
private struct QueuedPromptsView: View {
    let prompts: [QueuedPrompt]
    @State private var expanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button {
                withAnimation(.snappy(duration: 0.18)) { expanded.toggle() }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                        .font(.caption2)
                    Text("已排队 \(prompts.count) 条，本轮结束后依次发送")
                        .font(.caption)
                    Spacer()
                }
            }
            .foregroundStyle(.secondary)
            .accessibilityIdentifier("queuedPromptsToggle")
            if expanded {
                ForEach(prompts) { prompt in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(prompt.text)
                            .font(.caption)
                            .lineLimit(3)
                        Text(prompt.at, style: .time)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                    .padding(8)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Color.accentColor.opacity(0.10), in: RoundedRectangle(cornerRadius: 10))
                }
            }
        }
        .padding(8)
        .background(.background, in: RoundedRectangle(cornerRadius: 12))
    }
}

import SwiftUI

@main @MainActor
struct MiaoApp: App {
    @UIApplicationDelegateAdaptor(MiaoApplicationDelegate.self) private var delegate
    @State private var model = AppModel()
    @State private var agent = LocalAgent()
    // The remote workbench is the reason most people install the app; a fresh
    // install should not open on the on-device provider form. The last choice
    // still wins after the first switch.
    @AppStorage("miao.selectedTab") private var selectedTab = 1

    var body: some Scene {
        WindowGroup {
            // Two modes in one app: the phone on its own, and the owner's computer.
            TabView(selection: $selectedTab) {
                LocalView(agent: agent)
                    .tabItem { Label("本机", systemImage: "iphone.gen3") }
                    .tag(0)
                RemoteView(model: model)
                    .tabItem { Label("电脑", systemImage: "desktopcomputer") }
                    .tag(1)
                    .onAppear { AppNotifications.shared.bind(model) }
            }
        }
    }
}

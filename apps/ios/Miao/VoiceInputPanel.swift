import SwiftUI
import UIKit

/// The shared recording surface. It replaces the compact mic affordances while
/// a take is active so the level, elapsed time, cancel and finish actions are
/// never mistaken for the keyboard's own controls.
struct VoiceInputPanel: View {
    @Bindable var speech: SpeechInput
    let tint: Color
    let onCancel: () -> Void
    let onFinish: () -> Void

    @State private var pulsing = false

    var body: some View {
        VStack(spacing: 12) {
            HStack(spacing: 8) {
                Circle()
                    .fill(speech.requesting ? Color.secondary : Color.red)
                    .frame(width: 8, height: 8)
                    .opacity(pulsing ? 0.3 : 1)
                    .animation(.easeInOut(duration: 0.9).repeatForever(autoreverses: true), value: pulsing)
                Text(speech.requesting ? "正在准备麦克风…" : "正在听写")
                    .font(.footnote.weight(.semibold))
                Spacer()
                Text(speech.durationText)
                    .font(.footnote.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .accessibilityLabel("已录音 \(Int(speech.duration)) 秒")
                Text(speech.localeTitle)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            VoiceWaveform(level: speech.level, tint: tint)
                .frame(height: 44)
                .accessibilityHidden(true)
            Text(speech.transcript.isEmpty ? (speech.requesting ? "允许访问后即可开始" : "请开始说话") : speech.transcript)
                .font(.callout)
                .lineLimit(3)
                .foregroundStyle(speech.transcript.isEmpty ? .secondary : .primary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12).padding(.vertical, 10)
                .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 12))
                .animation(.easeOut(duration: 0.12), value: speech.transcript)
                .accessibilityIdentifier("voiceTranscript")
            HStack(spacing: 12) {
                Button("取消", role: .cancel, action: onCancel)
                    .buttonStyle(.bordered)
                    .controlSize(.large)
                    .accessibilityIdentifier("voiceCancel")
                Spacer()
                Text("停顿约 \(Int(SpeechInput.silenceLimit)) 秒自动完成")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                Button("完成", action: onFinish)
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .accessibilityIdentifier("voiceFinish")
            }
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 18))
        .overlay(RoundedRectangle(cornerRadius: 18).strokeBorder(.separator, lineWidth: 0.5))
        .padding(.horizontal, 12)
        .onAppear { pulsing = true }
        .onChange(of: speech.recording) { _, listening in
            if listening {
                UIAccessibility.post(notification: .announcement, argument: "开始听写，停顿两秒会自动完成")
            }
        }
    }
}

/// A soft level meter rather than a faked animation: the bars follow the audio
/// tap so silence and a muted microphone look different from speech.
struct VoiceWaveform: View {
    let level: Float
    let tint: Color
    private let bars = 32

    var body: some View {
        TimelineView(.animation(minimumInterval: 1.0 / 30.0)) { timeline in
            Canvas { context, size in
                let time = timeline.date.timeIntervalSinceReferenceDate
                let step = size.width / CGFloat(bars)
                for index in 0..<bars {
                    let wave = (sin(time * 5.4 + Double(index) * 0.52) + 1) / 2
                    let envelope = 0.45 + 0.55 * sin(Double(index) / Double(bars - 1) * .pi)
                    let amplitude = 0.10 + Double(level) * 1.6 * wave * envelope
                    let height = max(3, min(size.height, size.height * amplitude))
                    let rect = CGRect(x: CGFloat(index) * step + step * 0.18,
                                      y: (size.height - height) / 2,
                                      width: step * 0.64, height: height)
                    context.fill(Path(roundedRect: rect, cornerRadius: step * 0.3),
                                 with: .color(tint.opacity(0.82)))
                }
            }
        }
    }
}

/// Errors stay next to the composer, and a permission failure offers the only
/// place that can fix it instead of asking the operator to hunt through Settings.
struct VoiceInputNotice: View {
    @Bindable var speech: SpeechInput

    var body: some View {
        Group {
            if let message = speech.error {
                HStack(spacing: 8) {
                    Text(message)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if speech.needsSettings {
                        Button("打开设置") { openSettings() }
                            .font(.caption.weight(.semibold))
                            .accessibilityIdentifier("voiceSettings")
                    }
                }
            } else if let notice = speech.notice {
                Text(notice)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .onChange(of: speech.error) { _, value in announce(value) }
        .onChange(of: speech.notice) { _, value in announce(value) }
    }

    private func announce(_ value: String?) {
        guard let value else { return }
        UIAccessibility.post(notification: .announcement, argument: value)
    }

    private func openSettings() {
        guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
        UIApplication.shared.open(url)
    }
}

/// Tap starts a take; long press chooses the recognizer language. The choice is
/// persisted by `SpeechInput`, so it survives a relaunch.
struct SpeechMicButton: View {
    @Bindable var speech: SpeechInput
    let identifier: String
    let action: () -> Void

    var body: some View {
        Menu {
            Button { speech.selectLocale("") } label: {
                Text("跟随系统（\(speech.systemLocaleIdentifier)）")
            }
            Button { speech.selectLocale("zh-CN") } label: { Text("普通话") }
            Button { speech.selectLocale("en-US") } label: { Text("English") }
        } label: {
            Image(systemName: "mic")
                .font(.title3)
                .foregroundStyle(Color.accentColor)
        } primaryAction: {
            action()
        }
        .accessibilityIdentifier(identifier)
        .accessibilityLabel("语音输入")
        .accessibilityHint("轻点开始听写，长按选择识别语言")
        .disabled(speech.requesting)
    }
}

/// The visible language control, kept in the composer row while idle so the
/// option is discoverable without a long press.
struct SpeechLanguageMenu: View {
    @Bindable var speech: SpeechInput

    var body: some View {
        Menu {
            Button { speech.selectLocale("") } label: {
                Label("跟随系统（\(speech.systemLocaleIdentifier)）",
                      systemImage: speech.localeIdentifier.isEmpty ? "checkmark" : "globe")
            }
            Button { speech.selectLocale("zh-CN") } label: {
                Label("普通话", systemImage: speech.localeIdentifier == "zh-CN" ? "checkmark" : "character")
            }
            Button { speech.selectLocale("en-US") } label: {
                Label("English", systemImage: speech.localeIdentifier == "en-US" ? "checkmark" : "character")
            }
        } label: {
            Image(systemName: "globe").font(.title3)
        }
        .accessibilityLabel("语音识别语言")
        .accessibilityValue(speech.localeTitle)
        .disabled(speech.active)
    }
}

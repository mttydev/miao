import SwiftUI
import Speech
import AVFoundation
import Observation

@MainActor @Observable
final class SpeechInput {
    var recording = false
    var requesting = false
    var error: String?
    /// Empty means the device language. A selected identifier is passed to the
    /// recognizer before it starts, so changing it never restarts a live task.
    var localeIdentifier = ""
    private(set) var duration: TimeInterval = 0
    private let engine = AVAudioEngine()
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var task: SFSpeechRecognitionTask?
    private var generation = 0
    private var tapped = false
    private var expectedDraft = ""
    private var origin = ""
    private var startedAt: Date?
    private var ticker: Task<Void, Never>?
    private var interruptionObserver: NSObjectProtocol?
    private var routeObserver: NSObjectProtocol?

    var durationText: String {
        String(format: "%d:%02d", Int(duration) / 60, Int(duration) % 60)
    }

    /// The language system dictation would use. `Locale.autoupdatingCurrent`
    /// can follow the app's own (unlocalized) language, which made the
    /// "automatic" choice recognize English on a Chinese phone. The preferred
    /// language list is the user's system choice, independent of app resources.
    static var systemLocale: Locale {
        Locale(identifier: Locale.preferredLanguages.first ?? "en-US")
    }

    /// SFSpeechRecognizer names locales in BCP-47 form (`zh-CN`), while
    /// `Locale` often carries `zh_CN` or a script (`zh-Hans-CN`). Normalize
    /// before matching its supported list so a Chinese phone cannot silently
    /// fall back to English.
    static func normalizedIdentifier(_ locale: Locale) -> String {
        let language = locale.language.languageCode?.identifier ?? "en"
        let region = locale.region?.identifier
        if language == "zh" {
            let traditional = locale.language.script?.identifier == "Hant"
                || region == "TW"
                || region == "HK"
                || region == "MO"
            return traditional ? "zh-TW" : "zh-CN"
        }
        return region.map { "\(language)-\($0)" } ?? language
    }

    var systemLocaleIdentifier: String {
        Self.normalizedIdentifier(Self.systemLocale)
    }

    /// Recognition only edits the draft. It never submits a prompt or approval.
    func start(text: String, locale: String? = nil,
               accept: @escaping @MainActor (String, String) -> Bool) async {
        guard !recording, !requesting else { return }
        generation += 1; let current = generation
        requesting = true; error = nil
        let speech = await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
        }
        let microphone = await withCheckedContinuation { continuation in
            AVAudioApplication.requestRecordPermission { continuation.resume(returning: $0) }
        }
        guard generation == current else { return }
        requesting = false
        guard speech == .authorized, microphone else { error = "请在系统设置中允许麦克风和语音识别"; return }
        let selected = locale ?? localeIdentifier
        let requested = selected.isEmpty ? Self.systemLocale : Locale(identifier: selected)
        let identifier = Self.normalizedIdentifier(requested)
        let language = requested.language.languageCode?.identifier
        let supported = SFSpeechRecognizer.supportedLocales()
        let recognizerLocale = supported.first {
            $0.language.languageCode?.identifier == language
                && $0.region?.identifier == requested.region?.identifier
        } ?? supported.first {
            $0.language.languageCode?.identifier == language
        }
        guard let recognizerLocale else {
            error = "当前语言（\(identifier)）不支持离线语音识别，也可以使用系统键盘的听写功能"; return
        }
        guard let recognizer = SFSpeechRecognizer(locale: recognizerLocale), recognizer.isAvailable,
              recognizer.supportsOnDeviceRecognition else {
            error = "当前语言（\(identifier)）无法使用离线语音识别，也可以使用系统键盘的听写功能"; return
        }
        do {
            let audio = AVAudioSession.sharedInstance()
            // `duckOthers` is only legal for playAndRecord/playback/multiRoute;
            // this session only records, so asking to duck other audio made
            // setCategory fail before the engine could start.
            try audio.setCategory(.record, mode: .measurement)
            try audio.setActive(true)
            let request = SFSpeechAudioBufferRecognitionRequest()
            request.shouldReportPartialResults = true; request.requiresOnDeviceRecognition = true
            self.request = request
            let input = engine.inputNode
            let format = input.outputFormat(forBus: 0)
            guard format.sampleRate > 0, format.channelCount > 0 else { throw CocoaError(.featureUnsupported) }
            input.installTap(onBus: 0, bufferSize: 1024, format: format) { buffer, _ in request.append(buffer) }
            tapped = true
            engine.prepare(); try engine.start(); recording = true
            expectedDraft = text
            origin = text
            startedAt = Date()
            duration = 0
            startTicker()
            observeAudioSession()
            task = recognizer.recognitionTask(with: request) { [weak self] result, failure in
                let words = result?.bestTranscription.formattedString
                let finished = result?.isFinal == true || failure != nil
                Task { @MainActor in
                    guard let self, self.generation == current else { return }
                    if let words, !words.isEmpty {
                        let value = text + (text.isEmpty || text.hasSuffix(" ") || text.hasSuffix("\n") ? "" : " ") + words
                        guard accept(self.expectedDraft, value) else { self.stop(); self.error = "输入已更改，语音已停止"; return }
                        self.expectedDraft = value
                    }
                    guard finished else { return }
                    self.stop()
                    // A failure is not a normal end: without a message the
                    // button simply snapped back and nothing explained why.
                    if failure != nil {
                        self.error = "语音识别失败，请检查麦克风权限，或在语言菜单里换一种语言"
                    } else if self.expectedDraft == self.origin {
                        self.error = "没有识别到语音，请靠近麦克风重试"
                    }
                }
            }
        } catch { stop(); self.error = "语音输入暂时不可用" }
    }

    /// Stop keeps the recognized text. Cancel restores the draft captured when
    /// recording began, so a mis-tap never leaves half a sentence behind.
    func cancel(restore: @MainActor (String) -> Void) {
        let original = origin
        stop()
        restore(original)
    }

    func stop() {
        generation += 1; requesting = false; recording = false
        ticker?.cancel(); ticker = nil
        removeAudioObservers()
        startedAt = nil
        engine.stop()
        if tapped { engine.inputNode.removeTap(onBus: 0); tapped = false }
        request?.endAudio(); task?.cancel(); task = nil; request = nil
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    private func startTicker() {
        ticker?.cancel()
        ticker = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(200))
                guard let self, self.recording, let startedAt = self.startedAt else { return }
                self.duration = Date().timeIntervalSince(startedAt)
            }
        }
    }

    private func observeAudioSession() {
        removeAudioObservers()
        interruptionObserver = NotificationCenter.default.addObserver(
            forName: AVAudioSession.interruptionNotification, object: nil, queue: .main
        ) { [weak self] note in
            guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                  AVAudioSession.InterruptionType(rawValue: raw) == .began else { return }
            Task { @MainActor [weak self] in
                guard let self, self.recording else { return }
                self.stop()
                self.error = "录音被系统中断，已保留已识别文字"
            }
        }
        routeObserver = NotificationCenter.default.addObserver(
            forName: AVAudioSession.routeChangeNotification, object: nil, queue: .main
        ) { [weak self] note in
            guard let raw = note.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt,
                  AVAudioSession.RouteChangeReason(rawValue: raw) == .oldDeviceUnavailable else { return }
            Task { @MainActor [weak self] in
                guard let self, self.recording else { return }
                self.stop()
                self.error = "音频设备已断开，语音已停止"
            }
        }
    }

    private func removeAudioObservers() {
        if let interruptionObserver { NotificationCenter.default.removeObserver(interruptionObserver) }
        if let routeObserver { NotificationCenter.default.removeObserver(routeObserver) }
        interruptionObserver = nil
        routeObserver = nil
    }
}

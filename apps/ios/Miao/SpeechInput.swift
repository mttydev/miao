import SwiftUI
import Speech
import AVFoundation
import Observation
import MiaoCore

@MainActor @Observable
final class SpeechInput {
    var recording = false
    var requesting = false
    /// A recoverable failure, shown in red by the composer.
    var error: String?
    /// The failure can only be fixed in Settings, so the UI offers the shortcut.
    var needsSettings = false
    /// An informational outcome such as an automatic stop, not a failure.
    var notice: String?
    /// Smoothed input level, 0...1. The panel draws it so the operator can see
    /// that the microphone is actually live.
    private(set) var level: Float = 0
    /// Latest partial transcription of this take, before it is merged into the
    /// draft. The draft itself remains the editable source of truth.
    private(set) var transcript = ""
    /// Empty means the device language. Persisted so the choice survives a
    /// relaunch; the recognizer reads it before a task starts.
    var localeIdentifier = ""
    private(set) var duration: TimeInterval = 0

    private static let localeKey = "miao.speechLocale"
    /// A pause this long after speech ends the take, like system dictation.
    static let silenceLimit: TimeInterval = 2.6
    private static let durationLimit: TimeInterval = 120
    private static let noSpeechLimit: TimeInterval = 7

    private let engine = AVAudioEngine()
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var task: SFSpeechRecognitionTask?
    private var generation = 0
    private var tapped = false
    private var expectedDraft = ""
    private var origin = ""
    private var startedAt: Date?
    private var lastVoiceAt = Date.distantPast
    private var heardSpeech = false
    private var ticker: Task<Void, Never>?
    private var interruptionObserver: NSObjectProtocol?
    private var routeObserver: NSObjectProtocol?

    init() {
        localeIdentifier = UserDefaults.standard.string(forKey: Self.localeKey) ?? ""
    }

    func selectLocale(_ identifier: String) {
        localeIdentifier = identifier
        UserDefaults.standard.set(identifier, forKey: Self.localeKey)
    }

    var active: Bool { recording || requesting }

    var durationText: String {
        String(format: "%d:%02d", Int(duration) / 60, Int(duration) % 60)
    }

    var localeTitle: String {
        switch localeIdentifier {
        case "zh-CN": return "普通话"
        case "zh-TW": return "國語（台灣）"
        case "en-US": return "English"
        default: return "跟随系统（\(systemLocaleIdentifier)）"
        }
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
        generation += 1
        let current = generation
        // Bind the draft before asking for permission: cancelling while the
        // system prompt is up must restore what the user had, not a stale take.
        origin = text
        expectedDraft = text
        requesting = true
        error = nil
        notice = nil
        needsSettings = false
        transcript = ""
        level = 0
        duration = 0
        heardSpeech = false
        let speech = await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
        }
        let microphone = await withCheckedContinuation { continuation in
            AVAudioApplication.requestRecordPermission { continuation.resume(returning: $0) }
        }
        guard generation == current else { return }
        requesting = false
        guard speech == .authorized, microphone else {
            error = "请在系统设置中允许 miao 使用麦克风和语音识别"
            needsSettings = true
            return
        }
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
            error = "当前语言（\(identifier)）不支持离线语音识别，也可以使用系统键盘的听写功能"
            return
        }
        guard let recognizer = SFSpeechRecognizer(locale: recognizerLocale), recognizer.isAvailable,
              recognizer.supportsOnDeviceRecognition else {
            error = "当前语言（\(identifier)）无法使用离线语音识别，也可以使用系统键盘的听写功能"
            return
        }
        let chinese = recognizerLocale.language.languageCode?.identifier == "zh"
        do {
            let audio = AVAudioSession.sharedInstance()
            // `duckOthers` is only legal for playAndRecord/playback/multiRoute;
            // this session only records, so asking to duck other audio made
            // setCategory fail before the engine could start.
            try audio.setCategory(.record, mode: .measurement)
            try audio.setActive(true)
            let request = SFSpeechAudioBufferRecognitionRequest()
            request.shouldReportPartialResults = true
            request.requiresOnDeviceRecognition = true
            request.taskHint = .dictation
            request.addsPunctuation = true
            self.request = request
            let input = engine.inputNode
            let format = input.outputFormat(forBus: 0)
            guard format.sampleRate > 0, format.channelCount > 0 else { throw CocoaError(.featureUnsupported) }
            input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
                request.append(buffer)
                let level = Self.level(of: buffer)
                Task { @MainActor [weak self] in self?.push(level) }
            }
            tapped = true
            engine.prepare()
            try engine.start()
            recording = true
            startedAt = Date()
            lastVoiceAt = Date()
            duration = 0
            startTicker()
            observeAudioSession()
            task = recognizer.recognitionTask(with: request) { [weak self] result, failure in
                let raw = result?.bestTranscription.formattedString
                let finished = result?.isFinal == true || failure != nil
                Task { @MainActor [weak self] in
                    guard let self, self.generation == current else { return }
                    if let raw, !raw.isEmpty {
                        let words = DictationText.punctuate(raw, chinese: chinese)
                        self.transcript = words
                        // Recognition is authoritative even when the level
                        // meter stayed under its threshold on a quiet input.
                        self.heardSpeech = true
                        self.lastVoiceAt = Date()
                        let value = DictationText.merge(text, words)
                        guard accept(self.expectedDraft, value) else {
                            self.stop()
                            self.error = "输入已更改，语音已停止"
                            return
                        }
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
        } catch {
            stop()
            self.error = "无法启动麦克风，请检查是否被其他应用占用"
        }
    }

    /// Stop keeps the recognized text. Cancel restores the draft captured when
    /// recording began, so a mis-tap never leaves half a sentence behind.
    func cancel(restore: @MainActor (String) -> Void) {
        let original = origin
        stop()
        restore(original)
    }

    func stop() {
        generation += 1
        requesting = false
        recording = false
        ticker?.cancel()
        ticker = nil
        removeAudioObservers()
        startedAt = nil
        level = 0
        engine.stop()
        if tapped { engine.inputNode.removeTap(onBus: 0); tapped = false }
        request?.endAudio()
        task?.cancel()
        task = nil
        request = nil
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    private func push(_ raw: Float) {
        guard recording else { return }
        let value = min(1, max(0, raw * 16))
        level = max(value, level * 0.7)
        if raw > 0.004 {
            heardSpeech = true
            lastVoiceAt = Date()
        }
    }

    private func startTicker() {
        ticker?.cancel()
        ticker = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(120))
                guard let self, self.recording, let startedAt = self.startedAt else { return }
                self.duration = Date().timeIntervalSince(startedAt)
                self.level = self.level > 0.03 ? self.level * 0.82 : 0
                if self.duration > Self.durationLimit {
                    self.stop()
                    self.error = "已达到最长录音时间，文字已保留"
                    return
                }
                if !self.heardSpeech, self.expectedDraft == self.origin, self.duration > Self.noSpeechLimit {
                    self.stop()
                    self.error = "没有检测到语音，请靠近麦克风重试"
                    return
                }
                if self.heardSpeech, Date().timeIntervalSince(self.lastVoiceAt) > Self.silenceLimit {
                    self.stop()
                    self.notice = "停顿两秒，已自动完成"
                    return
                }
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
                self.notice = "录音被系统中断，已保留已识别文字"
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
                self.notice = "音频设备已断开，语音已停止"
            }
        }
    }

    private func removeAudioObservers() {
        if let interruptionObserver { NotificationCenter.default.removeObserver(interruptionObserver) }
        if let routeObserver { NotificationCenter.default.removeObserver(routeObserver) }
        interruptionObserver = nil
        routeObserver = nil
    }

    private nonisolated static func level(of buffer: AVAudioPCMBuffer) -> Float {
        guard let channels = buffer.floatChannelData, buffer.frameLength > 0 else { return 0 }
        let count = Int(buffer.frameLength)
        let samples = channels[0]
        var sum: Float = 0
        for index in 0..<count {
            let sample = samples[index]
            sum += sample * sample
        }
        return (sum / Float(count)).squareRoot()
    }

}

import Foundation

/// Text shaping for on-device dictation. It lives in the shared core so the
/// rules can be tested without a microphone or a device.
public enum DictationText {
    /// Join a recognized tail to the untouched draft. Chinese, Japanese and
    /// Korean do not take a space between characters, and a trailing
    /// punctuation mark already separates the next sentence.
    public static func merge(_ base: String, _ words: String) -> String {
        guard !words.isEmpty else { return base }
        guard !base.isEmpty else { return words }
        if base.hasSuffix(" ") || base.hasSuffix("\n") || base.hasSuffix("\t") {
            return base + words
        }
        if let last = base.unicodeScalars.last {
            let value = last.value
            let unspaced = (0x3000...0x303F).contains(value)
                || (0x3040...0x30FF).contains(value)
                || (0x31F0...0x31FF).contains(value)
                || (0x3400...0x4DBF).contains(value)
                || (0x4E00...0x9FFF).contains(value)
                || (0xF900...0xFAFF).contains(value)
                || (0xFF00...0xFFEF).contains(value)
                || (0xAC00...0xD7AF).contains(value)
            if unspaced || "，。！？；：、,.!?;:".unicodeScalars.contains(last) {
                return base + words
            }
        }
        return base + " " + words
    }

    /// Spoken punctuation is a dictation convention; without it a Chinese take
    /// arrives as one unpunctuated run and has to be retyped.
    public static func punctuate(_ words: String, chinese: Bool) -> String {
        guard chinese else { return words }
        let spoken = [
            ("逗号", "，"), ("句号", "。"), ("问号", "？"), ("感叹号", "！"),
            ("顿号", "、"), ("冒号", "："), ("分号", "；"), ("换行", "\n"),
        ]
        return spoken.reduce(words) { text, pair in
            text.replacingOccurrences(of: pair.0, with: pair.1)
        }
    }
}

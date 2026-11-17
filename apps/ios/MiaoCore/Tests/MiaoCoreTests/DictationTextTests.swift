import XCTest
@testable import MiaoCore

final class DictationTextTests: XCTestCase {
    func testEmptySideReturnsTheOther() {
        XCTAssertEqual(DictationText.merge("", "你好"), "你好")
        XCTAssertEqual(DictationText.merge("你好", ""), "你好")
    }

    func testEnglishDraftKeepsAWordBoundary() {
        XCTAssertEqual(DictationText.merge("deploy the", "staging build"), "deploy the staging build")
        XCTAssertEqual(DictationText.merge("deploy the ", "staging build"), "deploy the staging build")
    }

    func testCjkDraftDoesNotInsertASpace() {
        XCTAssertEqual(DictationText.merge("帮我检查", "这个改动"), "帮我检查这个改动")
        XCTAssertEqual(DictationText.merge("確認して", "ください"), "確認してください")
        XCTAssertEqual(DictationText.merge("다시", "확인해"), "다시확인해")
    }

    func testTrailingPunctuationSeparatesTheNextSentence() {
        XCTAssertEqual(DictationText.merge("你好。", "继续"), "你好。继续")
        XCTAssertEqual(DictationText.merge("Done!", "Next"), "Done!Next")
        XCTAssertEqual(DictationText.merge("第一行\n", "第二行"), "第一行\n第二行")
    }

    func testChineseSpokenPunctuationBecomesMarks() {
        XCTAssertEqual(DictationText.punctuate("你好逗号世界句号", chinese: true), "你好，世界。")
        XCTAssertEqual(DictationText.punctuate("真的吗问号", chinese: true), "真的吗？")
        XCTAssertEqual(DictationText.punctuate("第一行换行第二行", chinese: true), "第一行\n第二行")
    }

    func testSpokenPunctuationIsLeftAloneForOtherLanguages() {
        XCTAssertEqual(DictationText.punctuate("say 逗号 loudly", chinese: false), "say 逗号 loudly")
    }

    func testRepeatedPartialsStayStable() {
        let once = DictationText.punctuate("准备逗号开始", chinese: true)
        XCTAssertEqual(DictationText.punctuate(once, chinese: true), once)
    }
}

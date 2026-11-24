import AVFoundation
import Flutter
import Foundation

// Forward speech synthesizer callbacks to the Flutter engine.
final class SpeechHost: NSObject, AVSpeechSynthesizerDelegate {
  private let synthesizer = AVSpeechSynthesizer()
  private let channel: FlutterMethodChannel
  private var active: AVSpeechUtterance?
  private var identities: [ObjectIdentifier: Int] = [:]
  private var nextID = 0
  private var records = 0
  private let logURL: URL

  init(messenger: FlutterBinaryMessenger) {
    channel = FlutterMethodChannel(name: "dev.frontcap/speech", binaryMessenger: messenger)
    logURL = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
      .appendingPathComponent("native-speech.jsonl")
    super.init()
    synthesizer.delegate = self
    channel.setMethodCallHandler { [weak self] call, result in
      guard let self = self else { result(FlutterError(code: "disposed", message: nil, details: nil)); return }
      self.handle(call, result: result)
    }
    record("available", extra: ["voices": AVSpeechSynthesisVoice.speechVoices().map {
      ["language": $0.language, "identifier": $0.identifier]
    }])
  }

  private func record(_ kind: String, utterance: AVSpeechUtterance? = nil,
                      extra: [String: Any] = [:], feedback: Bool = false) {
    let current = utterance != nil && utterance === active
    var row: [String: Any] = [
      "kind": kind, "uptime_s": ProcessInfo.processInfo.systemUptime,
      "current": current, "speaking": synthesizer.isSpeaking, "paused": synthesizer.isPaused
    ]
    if let utterance = utterance {
      row["utterance_id"] = identities[ObjectIdentifier(utterance)]
      row["characters"] = utterance.speechString.count
    }
    row.merge(extra) { _, new in new }
    if records < 512 {
      records += 1
      row["sequence"] = records
      if records == 512 { row["limit_reached"] = true }
      do {
        var data = try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys])
        data.append(10)
        if !FileManager.default.fileExists(atPath: logURL.path) {
          FileManager.default.createFile(atPath: logURL.path, contents: nil)
        }
        let file = try FileHandle(forWritingTo: logURL)
        defer { try? file.close() }
        try file.seekToEnd()
        try file.write(contentsOf: data)
      } catch { NSLog("LOCAL_SPEECH log failure: %@", String(describing: error)) }
    }
    if feedback && current { channel.invokeMethod("event", arguments: row) }
  }

  private func handle(_ call: FlutterMethodCall, result: @escaping FlutterResult) {
    switch call.method {
    case "speak":
      guard let args = call.arguments as? [String: Any],
            let text = args["text"] as? String, !text.isEmpty, text.count <= 4000,
            let language = args["language"] as? String else {
        result(FlutterError(code: "invalid_speech", message: nil, details: nil)); return
      }
      if active != nil {
        record("replace_requested", utterance: active)
        synthesizer.stopSpeaking(at: .immediate)
      }
      let utterance = AVSpeechUtterance(string: text)
      nextID += 1
      identities[ObjectIdentifier(utterance)] = nextID
      active = utterance
      guard let voice = AVSpeechSynthesisVoice(language: language) else {
        record("error", utterance: utterance, extra: ["reason": "voice_unavailable", "language": language],
               feedback: true)
        identities.removeValue(forKey: ObjectIdentifier(utterance))
        active = nil
        result(["accepted": false, "utterance_id": nextID]); return
      }
      utterance.voice = voice
      record("speak_requested", utterance: utterance,
             extra: ["voice": voice.identifier, "language": voice.language])
      synthesizer.speak(utterance)
      result(["accepted": true, "utterance_id": nextID])
    case "stop":
      record("stop_requested", utterance: active)
      let accepted = synthesizer.stopSpeaking(at: .immediate)
      result(["accepted": accepted])
    case "pause":
      record("pause_requested", utterance: active)
      result(["accepted": synthesizer.pauseSpeaking(at: .immediate)])
    case "resume":
      record("resume_requested", utterance: active)
      result(["accepted": synthesizer.continueSpeaking()])
    default: result(FlutterMethodNotImplemented)
    }
  }

  func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didStart utterance: AVSpeechUtterance) {
    record("playing", utterance: utterance, feedback: true)
  }
  func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didFinish utterance: AVSpeechUtterance) {
    record("done", utterance: utterance, feedback: true)
    if utterance === active { active = nil }
    identities.removeValue(forKey: ObjectIdentifier(utterance))
  }
  func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didCancel utterance: AVSpeechUtterance) {
    record("stopped", utterance: utterance, feedback: true)
    if utterance === active { active = nil }
    identities.removeValue(forKey: ObjectIdentifier(utterance))
  }
  func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didPause utterance: AVSpeechUtterance) {
    record("paused", utterance: utterance, feedback: true)
  }
  func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didContinue utterance: AVSpeechUtterance) {
    record("resumed", utterance: utterance, feedback: true)
  }
}

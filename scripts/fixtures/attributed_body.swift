// Generate synthetic archives with Apple's encoder, independently of the Rust reader.
// No Messages database, crabstep code, or third-party fixture is used.
// Run on macOS: swift scripts/fixtures/attributed_body.swift
import AppKit

struct Sample: Encodable {
    let name: String
    let text: String
    let hex: String
}

var samples: [Sample] = []

func archive(_ name: String, _ value: NSAttributedString) {
    let data = NSArchiver.archivedData(withRootObject: value)
    // The native decoder is an oracle for these self-generated archives only.
    guard let decoded = NSUnarchiver.unarchiveObject(with: data) as? NSAttributedString,
          decoded.string == value.string else {
        fatalError("Native round trip failed: \(name)")
    }
    samples.append(Sample(name: name, text: value.string,
                          hex: data.map { String(format: "%02x", $0) }.joined()))
}

archive("plain", NSAttributedString(string: "msgmcp synthetic message"))
archive("mutable", NSMutableAttributedString(string: "Mutable backing text"))
archive("empty", NSAttributedString(string: ""))
archive("unicode", NSAttributedString(string: "Hello 👩🏽‍💻 — café e\u{301} 日本語 العربية"))
archive("multiline", NSAttributedString(string: "First line\nSecond line\tTabbed\r\nLast line"))
archive("nul", NSAttributedString(string: "Before\u{0}after"))
archive("length16", NSAttributedString(string: String(repeating: "é", count: 300)))
archive("length32", NSAttributedString(string: String(repeating: "x", count: 70_000)))
archive("placeholder", NSAttributedString(string: "Photo: \u{fffc} followed by text"))

let styled = NSMutableAttributedString(string: "Link and styled message")
styled.addAttributes([.font: NSFont.boldSystemFont(ofSize: 15),
                      .foregroundColor: NSColor.red,
                      .underlineStyle: NSUnderlineStyle.single.rawValue],
                     range: NSRange(location: 0, length: 4))
styled.addAttribute(.link, value: URL(string: "https://example.com/msgmcp")!,
                    range: NSRange(location: 0, length: 4))
archive("styled_link", styled)

let numbers: [NSNumber] = [NSNumber(value: -1), NSNumber(value: Int64(-9_000_000_000)),
                         NSNumber(value: UInt64.max), NSNumber(value: Float(3.5)),
                         NSNumber(value: 100.5), NSNumber(value: 2.0), NSNumber(value: true)]
archive("attribute_values", NSAttributedString(string: "The body, not its attributes", attributes: [
    NSAttributedString.Key("numbers"): numbers,
    NSAttributedString.Key("range"): NSValue(range: NSRange(location: 3, length: 4)),
    NSAttributedString.Key("bytes"): Data([0, 0x84, 0x85, 0x86, 0xff]),
    NSAttributedString.Key("emptyArray"): NSArray(),
    NSAttributedString.Key("nested"): ["label": "This is not the message body"],
]))

// Force string and object reference indices past the one-byte range. Reusing a
// value also exercises object references across the large attribute dictionary.
var many: [NSAttributedString.Key: Any] = [:]
let shared = NSString(string: "Repeated attribute value")
for i in 0..<300 {
    many[NSAttributedString.Key("synthetic-key-\(i)")] = (i % 2 == 0)
        ? shared : NSString(string: "synthetic-value-\(i)")
}
archive("wide_references", NSAttributedString(string: "Body with many attributes", attributes: many))

let attachment = NSMutableAttributedString(attachment: NSTextAttachment(
    data: Data([0, 1, 2, 3]), ofType: "public.data"))
attachment.append(NSAttributedString(string: " Attachment caption"))
archive("attachment", attachment)

let output = try JSONEncoder().encode(samples)
FileHandle.standardOutput.write(output)
FileHandle.standardOutput.write(Data([10]))

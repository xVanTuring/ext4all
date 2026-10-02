import Foundation
import XCTest

/// Helpers for building ext4 images with e2fsprogs.
enum TestImage {
    static var sbin: String {
        ProcessInfo.processInfo.environment["E2FSPROGS_SBIN"] ?? "/opt/homebrew/opt/e2fsprogs/sbin"
    }

    static var available: Bool {
        FileManager.default.isExecutableFile(atPath: sbin + "/mke2fs")
    }

    @discardableResult
    static func run(_ tool: String, _ args: [String]) throws -> (Int32, String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: sbin + "/" + tool)
        p.arguments = args
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        try p.run()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return (p.terminationStatus, String(decoding: data, as: UTF8.self))
    }

    /// Create a fresh image and return its path.
    static func make(sizeMB: Int = 32, options: [String] = ["-t", "ext4"], label: String = "swifttest") throws -> String
    {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ext4kit-tests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let path = dir.appendingPathComponent("fs.img").path
        FileManager.default.createFile(atPath: path, contents: nil)
        let h = try FileHandle(forWritingTo: URL(fileURLWithPath: path))
        try h.truncate(atOffset: UInt64(sizeMB) << 20)
        try h.close()
        let (rc, out) = try run("mke2fs", ["-F", "-q", "-L", label] + options + [path])
        XCTAssertEqual(rc, 0, out)
        return path
    }

    static func assertClean(_ path: String, file: StaticString = #filePath, line: UInt = #line) throws {
        let (rc, out) = try run("e2fsck", ["-fn", path])
        XCTAssertEqual(rc, 0, out, file: file, line: line)
    }
}

/// A `BlockIO` that enforces sector alignment, like a real disk.
final class StrictBlockIO: BlockIO {
    let inner: FileBlockIO
    var reads = 0
    var writes = 0
    var flushes = 0

    init(path: String, readOnly: Bool, sectorSize: UInt32 = 4096) throws {
        inner = try FileBlockIO(path: path, readOnly: readOnly, sectorSize: sectorSize)
    }

    var size: UInt64 { inner.size }
    var sectorSize: UInt32 { inner.sectorSize }
    var isReadOnly: Bool { inner.isReadOnly }

    func read(at offset: UInt64, into buffer: UnsafeMutableRawBufferPointer) throws {
        precondition(offset % UInt64(sectorSize) == 0 && buffer.count % Int(sectorSize) == 0, "unaligned read")
        reads += 1
        try inner.read(at: offset, into: buffer)
    }

    func write(at offset: UInt64, from buffer: UnsafeRawBufferPointer) throws {
        precondition(offset % UInt64(sectorSize) == 0 && buffer.count % Int(sectorSize) == 0, "unaligned write")
        writes += 1
        try inner.write(at: offset, from: buffer)
    }

    func flush() throws {
        flushes += 1
        try inner.flush()
    }
}

extension Data {
    static func pattern(_ count: Int, seed: UInt8) -> Data {
        Data((0..<count).map { UInt8(truncatingIfNeeded: $0 &* 31 &+ Int(seed)) })
    }
}

func posixCode(_ error: Error) -> POSIXErrorCode? {
    (error as? POSIXError)?.code
}

import Ext4FFI
import XCTest

/// LUKS volumes and fscrypt folders through the Swift bridge and the
/// extension's `Unlocker`, with the images Linux made for the engine's
/// tests (crates/ext4-core/tests/fixtures/crypt).
final class EncryptionTests: XCTestCase {
    /// Unpack a fixture into a temporary file.
    func fixture(_ name: String) throws -> String {
        let src = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .appendingPathComponent("../../crates/ext4-core/tests/fixtures/crypt/\(name).img.gz")
            .standardizedFileURL
        try XCTSkipUnless(FileManager.default.fileExists(atPath: src.path), "fixtures not found")
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ext4kit-crypt-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let out = dir.appendingPathComponent("\(name).img")
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/gzip")
        p.arguments = ["-dc", src.path]
        FileManager.default.createFile(atPath: out.path, contents: nil)
        p.standardOutput = try FileHandle(forWritingTo: out)
        try p.run()
        p.waitUntilExit()
        XCTAssertEqual(p.terminationStatus, 0)
        return out.path
    }

    func testLuksUnlockAndRemember() throws {
        let path = try fixture("luks2-pbkdf2-4k")
        let io = try FileBlockIO(path: path, readOnly: false)
        XCTAssertThrowsError(try Ext4Mount.probe(io))
        let luks = try XCTUnwrap(try Ext4Mount.luksProbe(io))
        XCTAssertEqual(luks.version, 2)
        XCTAssertTrue(luks.supported)
        XCTAssertEqual(luks.uuid.count, 36)

        let store = MemoryStore()
        var unlocker = Unlocker(store: store)
        XCTAssertFalse(unlocker.mayUnlock(luks))
        try store.addSecret(Data("wrong".utf8), kind: .passphrase, name: "")
        XCTAssertTrue(unlocker.mayUnlock(luks))
        XCTAssertNil(try unlocker.luksKey(io: io, volume: luks))
        // a passphrase that failed is not tried again
        XCTAssertFalse(unlocker.mayUnlock(luks))
        try store.addSecret(Data("luks test passphrase".utf8), kind: .passphrase, name: "")
        let key = try XCTUnwrap(try unlocker.luksKey(io: io, volume: luks))
        XCTAssertEqual(store.luksKey(uuid: luks.uuid), key)

        // the remembered key alone opens it, with no passphrase left
        for s in store.secrets() {
            try store.remove(s.id)
        }
        unlocker = Unlocker(store: store)
        XCTAssertTrue(unlocker.mayUnlock(luks))
        XCTAssertEqual(unlocker.rememberedLuksKey(io: io, volume: luks), key)
        let inner = try Ext4Mount.luksProbeInner(io, key: key)
        XCTAssertEqual(inner.label, "luksdata")
        let mount = try Ext4Mount(luks: io, key: key, readOnly: false)
        let hello = try mount.lookup(Ext4Mount.rootIno, Data("hello.txt".utf8))
        XCTAssertEqual(try mount.read(hello.ino, offset: 0, length: 64), Data("hello luks\n".utf8))
        _ = try mount.create(
            Ext4Mount.rootIno, Data("swift".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        try mount.unmount()

        // a key that does not match is refused
        XCTAssertFalse(try Ext4Mount.luksCheckKey(io, key: Data(repeating: 1, count: 64)))
        XCTAssertThrowsError(try Ext4Mount(luks: io, key: Data(repeating: 1, count: 64), readOnly: true)) {
            XCTAssertEqual(posixCode($0), .EACCES)
        }
        try store.setLuksKey(Data(repeating: 2, count: 64), uuid: luks.uuid, label: "")
        XCTAssertNil(Unlocker(store: store).rememberedLuksKey(io: io, volume: luks))
    }

    func testPlainExt4IsNotLuks() throws {
        try XCTSkipUnless(TestImage.available, "e2fsprogs not installed")
        let path = try TestImage.make()
        XCTAssertNil(try Ext4Mount.luksProbe(FileBlockIO(path: path, readOnly: true)))
        XCTAssertFalse(try Ext4Mount.probe(FileBlockIO(path: path, readOnly: true)).encrypt)
    }

    func testFscryptProtectorsUnlockAndRemember() throws {
        let path = try fixture("fscrypt-tool")
        let store = MemoryStore()
        try store.addSecret(Data("not it".utf8), kind: .passphrase, name: "")
        try store.addSecret(Data("fscrypt test passphrase".utf8), kind: .passphrase, name: "")
        var mount = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        let info = try mount.volumeInfo()
        XCTAssertTrue(info.encrypt)
        let secret = try mount.lookup(Ext4Mount.rootIno, Data("secret".utf8))
        XCTAssertThrowsError(try mount.lookup(secret.ino, Data("hello.txt".utf8)))
        XCTAssertEqual(Unlocker(store: store).unlockFscrypt(mount, info: info), 1)
        let hello = try mount.lookup(secret.ino, Data("hello.txt".utf8))
        XCTAssertNotEqual(hello.flags & UInt32(EXT4_FL_ENCRYPT), 0)
        XCTAssertFalse(hello.supportsKernelIO, "encrypted files never use kernel offloaded I/O")
        XCTAssertEqual(try mount.read(hello.ino, offset: 0, length: 64), Data("hello fscrypt\n".utf8))
        XCTAssertEqual(store.fscryptKeys(volume: info.uuid).count, 1)
        try mount.unmount()

        // next time the remembered key is enough
        for s in store.secrets() {
            try store.remove(s.id)
        }
        mount = try Ext4Mount(FileBlockIO(path: path, readOnly: true), readOnly: true)
        XCTAssertEqual(Unlocker(store: store).unlockFscrypt(mount, info: info), 1)
        XCTAssertNoThrow(try mount.lookup(secret.ino, Data("hello.txt".utf8)))
        // a raw key file opens the folder of the raw-key protector
        let rawdir = try mount.lookup(Ext4Mount.rootIno, Data("rawdir".utf8))
        XCTAssertThrowsError(try mount.lookup(rawdir.ino, Data("hello.txt".utf8)))
        // key_bytes(99, 32) of make-crypt-fixtures.py
        let raw = Data((0..<32).map { (i: Int) -> UInt8 in UInt8((3663 + i * 11 + 5) % 256) })
        try store.addSecret(raw, kind: .keyFile, name: "raw.key")
        Unlocker(store: store).unlockFscrypt(mount, info: info)
        XCTAssertNoThrow(try mount.lookup(rawdir.ino, Data("hello.txt".utf8)))
    }

    func testRawMasterKeyFiles() {
        let bin = Data((0..<64).map { UInt8($0) })
        XCTAssertEqual(Unlocker.rawMasterKey(bin), bin)
        let hex = Data((bin.map { String(format: "%02x", $0) }.joined() + "\n").utf8)
        XCTAssertEqual(Unlocker.rawMasterKey(hex), bin)
        XCTAssertNil(Unlocker.rawMasterKey(Data(count: 8)))
        XCTAssertNil(Unlocker.rawMasterKey(Data(count: 4096)))
    }
}

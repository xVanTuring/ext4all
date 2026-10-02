import Foundation
import Security
import os

/// Keychain access is logged by whichever process uses the store.
private let keyLog = Logger(subsystem: "tech.xvanturing.ext4", category: "keys")

/// Where secrets for encrypted volumes are kept. `KeychainStore` is the
/// real one; tests use an in-memory store.
public protocol SecretStore: AnyObject, Sendable {
    /// Passphrases and key files the user added, tried on encrypted
    /// volumes that have no remembered key.
    func secrets() -> [StoredSecret]
    func addSecret(_ data: Data, kind: StoredSecret.Kind, name: String) throws
    /// The volume key remembered for a LUKS volume.
    func luksKey(uuid: String) -> Data?
    func setLuksKey(_ key: Data, uuid: String, label: String) throws
    /// fscrypt master keys remembered for an ext4 volume.
    func fscryptKeys(volume: UUID) -> [Data]
    func addFscryptKeys(_ keys: [Data], volume: UUID, label: String) throws
    /// Everything stored, for display (without the secret bytes).
    func entries() -> [StoredEntry]
    func remove(_ id: String) throws
}

/// A passphrase or key file added by the user.
public struct StoredSecret: Identifiable, Equatable, Sendable {
    public enum Kind: String, Sendable {
        case passphrase
        case keyFile = "key-file"
    }

    public let id: String
    public let kind: Kind
    public let data: Data
}

/// One keychain entry as listed in the app.
public struct StoredEntry: Identifiable, Equatable, Sendable {
    public enum Kind: Sendable {
        case passphrase, keyFile, luksVolume, fscryptVolume
    }

    public let id: String
    public let kind: Kind
    /// Key file name, or the volume's label.
    public let name: String
    /// Volume UUID for remembered keys.
    public let volume: String?
    public let created: Date?
}

/// The keychain, shared by the app and the file system extension through
/// the `tech.xvanturing.ext4.shared` access group (both are signed with
/// it), in the data protection keychain, never synchronized, readable
/// after the first unlock since login.
public final class KeychainStore: SecretStore, @unchecked Sendable {
    public static let shared = KeychainStore()

    private let service = "tech.xvanturing.ext4"
    /// Remembered fscrypt keys are this long (the fscrypt tool's size).
    private let fscryptKeySize = 64

    private func base(_ account: String? = nil) -> [String: Any] {
        var q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecUseDataProtectionKeychain as String: true,
        ]
        if let account {
            q[kSecAttrAccount as String] = account
        }
        return q
    }

    private func data(_ account: String) -> Data? {
        var q = base(account)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        let st = SecItemCopyMatching(q as CFDictionary, &out)
        if st != errSecSuccess && st != errSecItemNotFound {
            keyLog.error("keychain read \(account, privacy: .public): \(st)")
        }
        return st == errSecSuccess ? out as? Data : nil
    }

    private func put(_ data: Data, account: String, label: String, description: String, comment: String) throws {
        let attrs: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrLabel as String: label,
            kSecAttrDescription as String: description,
            kSecAttrComment as String: comment,
        ]
        var st = SecItemUpdate(base(account) as CFDictionary, attrs as CFDictionary)
        if st == errSecItemNotFound {
            var add = base(account)
            add.merge(attrs) { $1 }
            add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            add[kSecAttrSynchronizable as String] = false
            st = SecItemAdd(add as CFDictionary, nil)
        }
        guard st == errSecSuccess else {
            keyLog.error("keychain write \(account, privacy: .public): \(st)")
            throw KeyStoreError(status: st)
        }
    }

    private func attributes() -> [[String: Any]] {
        var q = base()
        q[kSecReturnAttributes as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitAll
        var out: CFTypeRef?
        let st = SecItemCopyMatching(q as CFDictionary, &out)
        if st != errSecSuccess && st != errSecItemNotFound {
            keyLog.error("keychain list: \(st)")
        }
        return (out as? [[String: Any]]) ?? []
    }

    public func secrets() -> [StoredSecret] {
        attributes().compactMap { a in
            guard let account = a[kSecAttrAccount as String] as? String, account.hasPrefix("secret:"),
                let kind = StoredSecret.Kind(rawValue: a[kSecAttrDescription as String] as? String ?? ""),
                let d = data(account)
            else { return nil }
            return StoredSecret(id: account, kind: kind, data: d)
        }
    }

    public func addSecret(_ data: Data, kind: StoredSecret.Kind, name: String) throws {
        try put(
            data, account: "secret:\(UUID().uuidString)", label: "Ext4Kit: \(name)", description: kind.rawValue,
            comment: name)
    }

    public func luksKey(uuid: String) -> Data? {
        data("luks:\(uuid.lowercased())")
    }

    public func setLuksKey(_ key: Data, uuid: String, label: String) throws {
        try put(
            key, account: "luks:\(uuid.lowercased())", label: "Ext4Kit: LUKS volume key \(uuid)",
            description: "luks-volume", comment: label)
    }

    public func fscryptKeys(volume: UUID) -> [Data] {
        guard let d = data("fscrypt:\(volume.uuidString.lowercased())") else { return [] }
        return stride(from: 0, to: d.count - d.count % fscryptKeySize, by: fscryptKeySize).map {
            d.subdata(in: $0..<$0 + fscryptKeySize)
        }
    }

    public func addFscryptKeys(_ keys: [Data], volume: UUID, label: String) throws {
        var all = fscryptKeys(volume: volume)
        for k in keys where k.count == fscryptKeySize && !all.contains(k) {
            all.append(k)
        }
        try put(
            all.reduce(Data(), +), account: "fscrypt:\(volume.uuidString.lowercased())",
            label: "Ext4Kit: fscrypt keys of \(volume.uuidString)", description: "fscrypt-volume", comment: label)
    }

    public func entries() -> [StoredEntry] {
        attributes().compactMap { a in
            guard let account = a[kSecAttrAccount as String] as? String else { return nil }
            let comment = a[kSecAttrComment as String] as? String ?? ""
            let created = a[kSecAttrCreationDate as String] as? Date
            let parts = account.split(separator: ":", maxSplits: 1).map(String.init)
            guard parts.count == 2 else { return nil }
            switch (parts[0], a[kSecAttrDescription as String] as? String) {
            case ("secret", StoredSecret.Kind.passphrase.rawValue?):
                return StoredEntry(id: account, kind: .passphrase, name: comment, volume: nil, created: created)
            case ("secret", StoredSecret.Kind.keyFile.rawValue?):
                return StoredEntry(id: account, kind: .keyFile, name: comment, volume: nil, created: created)
            case ("luks", _):
                return StoredEntry(id: account, kind: .luksVolume, name: comment, volume: parts[1], created: created)
            case ("fscrypt", _):
                return StoredEntry(
                    id: account, kind: .fscryptVolume, name: comment, volume: parts[1], created: created)
            default:
                return nil
            }
        }
        .sorted { ($0.created ?? .distantPast) < ($1.created ?? .distantPast) }
    }

    public func remove(_ id: String) throws {
        let st = SecItemDelete(base(id) as CFDictionary)
        guard st == errSecSuccess || st == errSecItemNotFound else {
            throw KeyStoreError(status: st)
        }
    }
}

public struct KeyStoreError: LocalizedError {
    public let status: OSStatus
    public var errorDescription: String? {
        (SecCopyErrorMessageString(status, nil) as String?) ?? "keychain error \(status)"
    }
}

/// A store in memory (tests).
public final class MemoryStore: SecretStore, @unchecked Sendable {
    private let lock = NSLock()
    private var items: [(id: String, kind: StoredEntry.Kind, name: String, data: Data)] = []

    public init() {}

    public func secrets() -> [StoredSecret] {
        lock.withLock {
            items.compactMap {
                switch $0.kind {
                case .passphrase: return StoredSecret(id: $0.id, kind: .passphrase, data: $0.data)
                case .keyFile: return StoredSecret(id: $0.id, kind: .keyFile, data: $0.data)
                default: return nil
                }
            }
        }
    }

    public func addSecret(_ data: Data, kind: StoredSecret.Kind, name: String) throws {
        lock.withLock {
            items.append(("secret:\(UUID().uuidString)", kind == .passphrase ? .passphrase : .keyFile, name, data))
        }
    }

    private func find(_ id: String) -> Data? {
        lock.withLock { items.first { $0.id == id }?.data }
    }

    private func set(_ id: String, _ kind: StoredEntry.Kind, _ name: String, _ data: Data) {
        lock.withLock {
            items.removeAll { $0.id == id }
            items.append((id, kind, name, data))
        }
    }

    public func luksKey(uuid: String) -> Data? { find("luks:\(uuid.lowercased())") }

    public func setLuksKey(_ key: Data, uuid: String, label: String) throws {
        set("luks:\(uuid.lowercased())", .luksVolume, label, key)
    }

    public func fscryptKeys(volume: UUID) -> [Data] {
        guard let d = find("fscrypt:\(volume.uuidString.lowercased())") else { return [] }
        return stride(from: 0, to: d.count, by: 64).map { d.subdata(in: $0..<min($0 + 64, d.count)) }
    }

    public func addFscryptKeys(_ keys: [Data], volume: UUID, label: String) throws {
        var all = fscryptKeys(volume: volume)
        for k in keys where !all.contains(k) {
            all.append(k)
        }
        set("fscrypt:\(volume.uuidString.lowercased())", .fscryptVolume, label, all.reduce(Data(), +))
    }

    public func entries() -> [StoredEntry] {
        lock.withLock {
            items.map {
                StoredEntry(
                    id: $0.id, kind: $0.kind, name: $0.name,
                    volume: $0.id.hasPrefix("secret:") ? nil : String($0.id.split(separator: ":")[1]),
                    created: nil)
            }
        }
    }

    public func remove(_ id: String) throws {
        lock.withLock { items.removeAll { $0.id == id } }
    }
}

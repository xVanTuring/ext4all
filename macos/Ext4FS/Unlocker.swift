import Foundation

/// Finds the keys of encrypted volumes: remembered ones first, then the
/// passphrases and key files the user added in the app, remembering what
/// works so later mounts skip the slow key derivation.
final class Unlocker: @unchecked Sendable {
    let store: SecretStore
    /// Where "volume|secret" pairs that did not open a volume are kept.
    /// FSKit runs probing, checking and mounting in separate processes, so
    /// they are kept on disk (secret ids only, never secrets): a key
    /// derivation can take seconds, and a LUKS volume no secret opens is
    /// then only recognized instead of failing every mount.
    private let defaults: UserDefaults?
    private static let failedKey = "FailedSecrets"
    private let failed: Locked<Set<String>>

    init(store: SecretStore, defaults: UserDefaults? = nil) {
        self.store = store
        self.defaults = defaults
        failed = Locked(Set(defaults?.stringArray(forKey: Self.failedKey) ?? []))
    }

    /// The failure set, refreshed from disk: other processes add to it.
    private func refreshed(_ f: inout Set<String>) {
        if let saved = defaults?.stringArray(forKey: Self.failedKey) {
            f.formUnion(saved)
        }
    }

    private func untried(_ volume: String) -> [StoredSecret] {
        let secrets = store.secrets()
        let tried = failed.withLock { f in
            refreshed(&f)
            // forget pairs of secrets that were removed
            let ids = Set(secrets.map(\.id))
            let kept = f.filter { ids.contains(String($0.split(separator: "|").last ?? "")) }
            if kept.count != f.count {
                f = kept
                defaults?.set(Array(kept), forKey: Self.failedKey)
            }
            return f
        }
        return secrets.filter { !tried.contains("\(volume)|\($0.id)") }
    }

    private func markFailed(_ volume: String, _ secret: StoredSecret) {
        failed.withLock { f in
            refreshed(&f)
            f.insert("\(volume)|\(secret.id)")
            defaults?.set(Array(f), forKey: Self.failedKey)
        }
    }

    // MARK: LUKS

    /// Whether a LUKS volume may open: a remembered key, or user secrets
    /// not yet tried on it.
    func mayUnlock(_ luks: Ext4LuksVolume) -> Bool {
        store.luksKey(uuid: luks.uuid) != nil || !untried("luks:\(luks.uuid)").isEmpty
    }

    /// The remembered key of a LUKS volume, if it still opens it.
    func rememberedLuksKey(io: BlockIO, volume luks: Ext4LuksVolume) -> Data? {
        guard let k = store.luksKey(uuid: luks.uuid) else { return nil }
        if (try? Ext4Mount.luksCheckKey(io, key: k)) == true {
            return k
        }
        Log.fs.info("the remembered key no longer opens LUKS volume \(luks.uuid, privacy: .public)")
        return nil
    }

    /// The volume key of a LUKS device: remembered, or recovered with one
    /// of the user's secrets (seconds each) and then remembered.
    func luksKey(io: BlockIO, volume luks: Ext4LuksVolume) throws -> Data? {
        if let k = rememberedLuksKey(io: io, volume: luks) {
            return k
        }
        let tag = "luks:\(luks.uuid)"
        for s in untried(tag) {
            let started = Date()
            if let k = try Ext4Mount.luksUnlock(io, passphrase: s.data) {
                Log.fs.info(
                    "LUKS volume \(luks.uuid, privacy: .public) unlocked in \(Date().timeIntervalSince(started), format: .fixed(precision: 1)) s"
                )
                do {
                    try store.setLuksKey(k, uuid: luks.uuid, label: luks.label)
                } catch {
                    Log.fs.error("cannot remember the LUKS key: \(error.localizedDescription, privacy: .public)")
                }
                return k
            }
            markFailed(tag, s)
        }
        return nil
    }

    // MARK: fscrypt

    /// Add fscrypt keys to a freshly mounted volume, before FSKit sees any
    /// name: remembered keys, key files holding raw master keys, and the
    /// user's secrets tried on the Linux `fscrypt` tool's protectors (what
    /// they open is remembered). Returns how many keys were added.
    @discardableResult
    func unlockFscrypt(_ mount: Ext4Mount, info: Ext4VolumeInfo) -> Int {
        var added = 0
        for k in store.fscryptKeys(volume: info.uuid) where (try? mount.addKey(k)) != nil {
            added += 1
        }
        let secrets = store.secrets()
        for s in secrets where s.kind == .keyFile {
            if let raw = Self.rawMasterKey(s.data), (try? mount.addKey(raw)) != nil {
                added += 1
            }
        }
        let tag = "fscrypt:\(info.uuid.uuidString)"
        for s in untried(tag) {
            do {
                let keys = try mount.unlockProtector(s.data)
                if keys.isEmpty {
                    markFailed(tag, s)
                    continue
                }
                added += keys.count
                try store.addFscryptKeys(keys, volume: info.uuid, label: info.label)
            } catch {
                Log.fs.error("fscrypt protectors: \(error.localizedDescription, privacy: .public)")
            }
        }
        if added > 0 {
            Log.fs.info("added \(added) fscrypt keys to \(info.uuid.uuidString, privacy: .public)")
        }
        return added
    }

    /// A key file holding an fscrypt master key: 16 to 64 bytes, raw or
    /// as hexadecimal text.
    static func rawMasterKey(_ d: Data) -> Data? {
        if let text = String(data: d, encoding: .ascii)?.trimmingCharacters(in: .whitespacesAndNewlines),
            text.count % 2 == 0, (32...128).contains(text.count), text.allSatisfy(\.isHexDigit)
        {
            var out = Data()
            var i = text.startIndex
            while i < text.endIndex {
                let j = text.index(i, offsetBy: 2)
                out.append(UInt8(text[i..<j], radix: 16)!)
                i = j
            }
            return out
        }
        return (16...64).contains(d.count) ? d : nil
    }
}

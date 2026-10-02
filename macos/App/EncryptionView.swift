import AppKit
import SwiftUI

/// Passphrases and key files for encrypted disks (LUKS volumes and fscrypt
/// folders), and the keys remembered for disks they opened. Everything is
/// kept in the keychain shared with the file system extension.
struct EncryptionView: View {
    private let store: SecretStore = KeychainStore.shared
    @State private var entries: [StoredEntry] = []
    @State private var passphrase = ""
    @State private var message: String?
    @State private var failure: String?

    /// cryptsetup's limit for key files
    private let maxKeyFile = 8 << 20

    var body: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 12) {
                Text(
                    L(
                        "For LUKS-encrypted disks and fscrypt-encrypted folders. When such a disk is connected, Ext4Kit tries these passphrases and key files, then remembers the key of that disk in your keychain."
                    )
                )
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

                HStack {
                    SecureField(L("Passphrase"), text: $passphrase)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit(addPassphrase)
                    Button(L("Add"), action: addPassphrase)
                        .disabled(passphrase.isEmpty)
                    Button(L("Add Key File…"), action: addKeyFile)
                }

                if let message {
                    Label(message, systemImage: "info.circle")
                        .font(.callout)
                        .fixedSize(horizontal: false, vertical: true)
                }

                if entries.isEmpty {
                    Text(L("Nothing saved yet."))
                        .font(.callout)
                        .foregroundStyle(.tertiary)
                } else {
                    VStack(spacing: 0) {
                        ForEach(entries) { e in
                            row(e)
                            if e.id != entries.last?.id {
                                Divider()
                            }
                        }
                    }
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color(nsColor: .textBackgroundColor)))
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(4)
        } label: {
            Text(L("Encrypted disks"))
        }
        .onAppear(perform: reload)
        .alert(
            L("Could not update the keychain"),
            isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })
        ) {
            Button(L("OK")) { failure = nil }
        } message: {
            Text(failure ?? "")
        }
    }

    private func row(_ e: StoredEntry) -> some View {
        HStack(spacing: 10) {
            Image(systemName: icon(e.kind))
                .frame(width: 20)
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(title(e))
                Text(detail(e))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            Spacer()
            Button {
                remove(e)
            } label: {
                Image(systemName: "trash")
            }
            .buttonStyle(.borderless)
            .help(L("Remove"))
        }
        .padding(8)
    }

    private func icon(_ k: StoredEntry.Kind) -> String {
        switch k {
        case .passphrase: return "key"
        case .keyFile: return "doc.badge.ellipsis"
        case .luksVolume: return "externaldrive.fill.badge.checkmark"
        case .fscryptVolume: return "folder.badge.gearshape"
        }
    }

    private func title(_ e: StoredEntry) -> String {
        switch e.kind {
        case .passphrase:
            return L("Passphrase")
        case .keyFile:
            return String(format: L("Key file “%@”"), e.name)
        case .luksVolume:
            return e.name.isEmpty ? L("LUKS disk") : String(format: L("LUKS disk “%@”"), e.name)
        case .fscryptVolume:
            return e.name.isEmpty
                ? L("Encrypted folders") : String(format: L("Encrypted folders on “%@”"), e.name)
        }
    }

    private func detail(_ e: StoredEntry) -> String {
        var parts: [String] = []
        switch e.kind {
        case .passphrase, .keyFile:
            parts.append(L("Tried on encrypted disks without a remembered key"))
        case .luksVolume, .fscryptVolume:
            parts.append(L("Remembered key"))
            if let v = e.volume {
                parts.append(v)
            }
        }
        if let d = e.created {
            parts.append(d.formatted(date: .abbreviated, time: .shortened))
        }
        return parts.joined(separator: " · ")
    }

    private func reload() {
        entries = store.entries()
    }

    private func added() {
        message = L(
            "Saved. Connect the disk now; if it is already connected, eject it and connect it again. The first unlock of a LUKS disk can take several seconds."
        )
        reload()
    }

    private func addPassphrase() {
        guard !passphrase.isEmpty else { return }
        do {
            try store.addSecret(Data(passphrase.utf8), kind: .passphrase, name: "")
            passphrase = ""
            added()
        } catch {
            failure = error.localizedDescription
        }
    }

    private func addKeyFile() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = L("Choose a key file (a LUKS key file, or an fscrypt key).")
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            let data = try Data(contentsOf: url)
            guard !data.isEmpty, data.count <= maxKeyFile else {
                failure = L("A key file must hold between 1 byte and 8 MiB.")
                return
            }
            try store.addSecret(data, kind: .keyFile, name: url.lastPathComponent)
            added()
        } catch {
            failure = error.localizedDescription
        }
    }

    private func remove(_ e: StoredEntry) {
        do {
            try store.remove(e.id)
            message = nil
            reload()
        } catch {
            failure = error.localizedDescription
        }
    }
}

import AppKit
import SwiftUI

struct ContentView: View {
    @State private var copied: String?

    private let settingsURL = URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!

    private var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?"
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                header
                step(
                    number: 1,
                    title: L("Enable the file system extension"),
                    detail: L(
                        "Open System Settings › General › Login Items & Extensions › File System Extensions and turn on Ext4Kit."
                    )
                ) {
                    Button(L("Open System Settings")) {
                        NSWorkspace.shared.open(settingsURL)
                    }
                }
                step(
                    number: 2,
                    title: L("Connect an ext4 disk"),
                    detail: L(
                        "Disks formatted with ext4, ext3 or ext2 are mounted automatically once the extension is enabled."
                    )
                ) {
                    EmptyView()
                }
                step(
                    number: 3,
                    title: L("Mount manually (optional)"),
                    detail: L("To mount a partition or a disk image yourself, run in Terminal:")
                ) {
                    VStack(alignment: .leading, spacing: 8) {
                        command("diskutil list")
                        command("mkdir -p /tmp/ext4 && mount -F -t ext4 disk4s1 /tmp/ext4")
                        command("mount -F -t ext4 -o ro disk4s1 /tmp/ext4")
                        command("hdiutil attach -nomount linux.img")
                    }
                }
                notes
            }
            .padding(28)
        }
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 16) {
            Image(systemName: "externaldrive.fill.badge.checkmark")
                .font(.system(size: 44))
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 4) {
                Text("Ext4Kit")
                    .font(.largeTitle.bold())
                Text(L("Read and write Linux ext4 disks on your Mac"))
                    .foregroundStyle(.secondary)
                Text(String(format: L("Version %@"), version))
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    private var notes: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 6) {
                Label(
                    L("Changes are journaled: an interrupted write never corrupts the file system."),
                    systemImage: "checkmark.shield")
                Label(L("Always eject the disk before unplugging it."), systemImage: "eject")
                Label(
                    L("Encrypted, compressed or casefolded ext4 volumes are mounted read-only."), systemImage: "lock")
            }
            .font(.callout)
            .frame(maxWidth: .infinity, alignment: .leading)
        } label: {
            Text(L("Good to know"))
        }
    }

    private func step<Content: View>(
        number: Int, title: String, detail: String, @ViewBuilder content: () -> Content
    ) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Text("\(number)")
                .font(.headline)
                .frame(width: 28, height: 28)
                .background(Circle().fill(Color.accentColor.opacity(0.15)))
            VStack(alignment: .leading, spacing: 6) {
                Text(title).font(.headline)
                Text(detail).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                content()
            }
        }
    }

    private func command(_ text: String) -> some View {
        HStack {
            Text(text)
                .font(.system(.body, design: .monospaced))
                .textSelection(.enabled)
            Spacer()
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
                copied = text
            } label: {
                Image(systemName: copied == text ? "checkmark" : "doc.on.doc")
            }
            .buttonStyle(.borderless)
            .help(L("Copy"))
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color(nsColor: .textBackgroundColor)))
    }
}

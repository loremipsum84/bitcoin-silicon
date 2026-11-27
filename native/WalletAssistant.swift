import AppKit
import Foundation

final class WalletAssistant: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private let chain: String
    private let miner: URL
    private let password = NSSecureTextField()
    private let confirmation = NSSecureTextField()
    private let button = NSButton(title: "Choose backup location…", target: nil, action: nil)
    private let status = NSTextField(wrappingLabelWithString: "")
    private var window: NSWindow!
    private var working = false

    init(chain: String, miner: URL) {
        self.chain = chain
        self.miner = miner
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let title = NSTextField(labelWithString: "Create a Bitcoin wallet")
        title.font = .systemFont(ofSize: 24, weight: .semibold)
        let network = chain == "mainnet" ? "Mainnet" : "Regtest"
        let details = NSTextField(wrappingLabelWithString:
            "Network is \(network). Bitcoin Core creates and restores an encrypted backup. Store the passphrase separately, for example in your password manager. You will need it to spend the funds.")
        password.placeholderString = "Passphrase (at least 12 characters)"
        confirmation.placeholderString = "Confirm passphrase"
        button.target = self
        button.action = #selector(chooseBackup)
        button.keyEquivalent = "\r"
        status.textColor = .secondaryLabelColor
        let stack = NSStackView(views: [title, details, password, confirmation, button, status])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.translatesAutoresizingMaskIntoConstraints = false

        let content = NSView(frame: NSRect(x: 0, y: 0, width: 530, height: 300))
        content.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 26),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -26),
            stack.topAnchor.constraint(equalTo: content.topAnchor, constant: 24),
            details.widthAnchor.constraint(equalTo: stack.widthAnchor),
            password.widthAnchor.constraint(equalTo: stack.widthAnchor),
            confirmation.widthAnchor.constraint(equalTo: stack.widthAnchor),
            status.widthAnchor.constraint(equalTo: stack.widthAnchor),
        ])
        window = NSWindow(contentRect: content.frame, styleMask: [.titled, .closable],
                          backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.title = "Bitcoin wallet · \(network)"
        window.contentView = content
        window.delegate = self
        window.center()
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(password)
        NSApp.activate(ignoringOtherApps: true)
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool { !working }
    func windowWillClose(_ notification: Notification) { NSApp.terminate(nil) }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        working ? .terminateCancel : .terminateNow
    }

    @objc private func chooseBackup() {
        let secret = password.stringValue
        guard secret.count >= 12, secret.utf8.count <= 512,
              !secret.utf8.contains(where: { $0 == 0 || $0 == 10 || $0 == 13 }) else {
            status.stringValue = "Enter at least 12 characters with no line breaks."
            return
        }
        guard secret == confirmation.stringValue else {
            status.stringValue = "The passphrases do not match."
            return
        }
        let panel = NSSavePanel()
        panel.title = "Save encrypted backup"
        panel.nameFieldStringValue = "bitcoin-\(chain)-wallet.dat"
        panel.canCreateDirectories = true
        panel.directoryURL = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first
        panel.beginSheetModal(for: window) { [self] result in
            guard result == .OK, let destination = panel.url else { return }
            password.stringValue = ""
            confirmation.stringValue = ""
            start(destination: destination, secret: Data(secret.utf8))
        }
    }

    private func start(destination: URL, secret: Data) {
        working = true
        password.isEnabled = false
        confirmation.isEnabled = false
        button.isEnabled = false
        status.stringValue = "Creating, backing up, and restoring the wallet…"
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            let task = Process()
            let input = Pipe()
            let output = Pipe()
            let errorOutput = Pipe()
            task.executableURL = miner
            task.arguments = ["wallet-create", chain, destination.path]
            task.standardInput = input
            task.standardOutput = output
            task.standardError = errorOutput

            do {
                try task.run()
                try input.fileHandleForWriting.write(contentsOf: secret)
                try input.fileHandleForWriting.close()
                task.waitUntilExit()
                let data = output.fileHandleForReading.readDataToEndOfFile()
                let text = String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
                let diagnostic = String(decoding: errorOutput.fileHandleForReading.readDataToEndOfFile(),
                                        as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
                DispatchQueue.main.async {
                    self.finished(address: task.terminationStatus == 0 ? text : nil, diagnostic: diagnostic)
                }
            } catch {
                try? input.fileHandleForWriting.close()
                if task.isRunning { task.waitUntilExit() }
                DispatchQueue.main.async { self.finished(address: nil, diagnostic: "Could not run the Bitcoin Core assistant.") }
            }
        }
    }

    private func finished(address: String?, diagnostic: String) {
        working = false
        guard let address, !address.isEmpty else {
            password.isEnabled = true
            confirmation.isEnabled = true
            button.isEnabled = true
            status.stringValue = diagnostic.isEmpty ? "Could not create or restore the wallet." : diagnostic
            return
        }
        let alert = NSAlert()
        alert.messageText = "Encrypted backup verified"
        alert.informativeText = "Bitcoin Core found the address in the restored backup. Store the passphrase separately.\n\n\(address)"
        alert.addButton(withTitle: "Use public address")
        alert.beginSheetModal(for: window) { _ in
            FileHandle.standardOutput.write(Data((address + "\n").utf8))
            NSApp.terminate(nil)
        }
    }
}

guard CommandLine.arguments.count == 3,
      ["mainnet", "regtest"].contains(CommandLine.arguments[1]) else { exit(2) }
let app = NSApplication.shared
let delegate = WalletAssistant(chain: CommandLine.arguments[1],
                               miner: URL(fileURLWithPath: CommandLine.arguments[2]))
app.setActivationPolicy(.regular)
app.delegate = delegate
app.run()

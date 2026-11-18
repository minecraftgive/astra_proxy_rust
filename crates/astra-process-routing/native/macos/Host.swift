import Foundation
import NetworkExtension
import SystemExtensions

final class ExtensionRequest: NSObject, OSSystemExtensionRequestDelegate {
    var continuation: CheckedContinuation<Void, Error>?

    func submit(id: String, remove: Bool) async throws {
        try await withCheckedThrowingContinuation { continuation in
            self.continuation = continuation
            let request = remove ? OSSystemExtensionRequest.deactivationRequest(forExtensionWithIdentifier: id, queue: .main)
                                 : OSSystemExtensionRequest.activationRequest(forExtensionWithIdentifier: id, queue: .main)
            request.delegate = self
            OSSystemExtensionManager.shared.submitRequest(request)
        }
    }

    func requestNeedsUserApproval(_ request: OSSystemExtensionRequest) {
        FileHandle.standardError.write(Data("Approve Astra Process Routing in System Settings.\n".utf8))
    }

    func request(_ request: OSSystemExtensionRequest, actionForReplacingExtension existing: OSSystemExtensionProperties,
                 withExtension ext: OSSystemExtensionProperties) -> OSSystemExtensionRequest.ReplacementAction { .replace }

    func request(_ request: OSSystemExtensionRequest, didFinishWithResult result: OSSystemExtensionRequest.Result) {
        if result == .completed { continuation?.resume() }
        else { continuation?.resume(throwing: POSIXError(.EAGAIN)) }
        continuation = nil
    }

    func request(_ request: OSSystemExtensionRequest, didFailWithError error: Error) {
        continuation?.resume(throwing: error)
        continuation = nil
    }
}

@main
struct Host {
    static func publish(_ data: Data, to session: NETunnelProviderSession) async throws {
        guard session.status == .connected else { throw POSIXError(.ENOTCONN) }
        let reply: Data? = try await withCheckedThrowingContinuation { continuation in
            do { try session.sendProviderMessage(data) { continuation.resume(returning: $0) } }
            catch { continuation.resume(throwing: error) }
        }
        guard reply == Data([1]) else { throw POSIXError(.EINVAL) }
    }

    static func main() async throws {
        let args = Array(CommandLine.arguments.dropFirst())
        guard let bundle = Bundle.main.bundleIdentifier else { throw POSIXError(.ENOENT) }
        let id = bundle + ".extension"
        guard let action = args.first, ["activate", "remove", "apply", "clear", "status"].contains(action) else {
            FileHandle.standardError.write(Data("Usage: AstraProcessRouting activate | apply RULES.json | status | clear | remove\n".utf8))
            throw POSIXError(.EINVAL)
        }
        if action == "activate" {
            try await ExtensionRequest().submit(id: id, remove: false)
            return
        }
        let configs = try await NETransparentProxyManager.loadAllFromPreferences()
        let previous = configs.first { ($0.protocolConfiguration as? NETunnelProviderProtocol)?.providerBundleIdentifier == id }
        if action == "status" {
            if let previous { print(previous.connection.status.rawValue) }
            else { print("absent") }
            return
        }
        if action == "clear" || action == "remove" {
            if let previous {
                previous.connection.stopVPNTunnel()
                try await previous.removeFromPreferences()
            }
            if action == "remove" { try await ExtensionRequest().submit(id: id, remove: true) }
            return
        }
        guard args.count == 2 else { throw POSIXError(.EINVAL) }
        let data = try Data(contentsOf: URL(fileURLWithPath: args[1]))
        _ = try checkedBindings(data)
        let manager = previous ?? NETransparentProxyManager()
        let savedProtocol = manager.protocolConfiguration
        let savedEnabled = manager.isEnabled
        let savedDescription = manager.localizedDescription
        let savedBindings = (savedProtocol as? NETunnelProviderProtocol)?.providerConfiguration?["bindings"] as? Data
        let wasConnected = manager.connection.status == .connected
        // A running provider acknowledges the whole candidate atomically. Old flows
        // hold their Binding by value; replacing the dictionary only affects new flows.
        if wasConnected {
            guard let session = manager.connection as? NETunnelProviderSession, savedBindings != nil
            else { throw POSIXError(.EINVAL) }
            try await publish(data, to: session)
        }
        let proto = NETunnelProviderProtocol()
        proto.providerBundleIdentifier = id
        proto.serverAddress = "Astra Process Routing"
        proto.providerConfiguration = ["bindings": data]
        manager.localizedDescription = "Astra Process Routing"
        manager.protocolConfiguration = proto
        manager.isEnabled = true
        var saved = false
        do {
            try await manager.saveToPreferences()
            saved = true
            if wasConnected { return }
            try await manager.loadFromPreferences()
            guard let session = manager.connection as? NETunnelProviderSession else { throw POSIXError(.ENOTCONN) }
            if session.status == .disconnected || session.status == .invalid { try session.startTunnel() }
            for _ in 0..<100 {
                if session.status == .connected { break }
                try await Task.sleep(nanoseconds: 100_000_000)
            }
            try await publish(data, to: session)
        } catch {
            let failure = error
            if wasConnected, let session = manager.connection as? NETunnelProviderSession, let savedBindings {
                try await publish(savedBindings, to: session)
            } else if saved {
                manager.connection.stopVPNTunnel()
                if previous != nil {
                    manager.protocolConfiguration = savedProtocol
                    manager.isEnabled = savedEnabled
                    manager.localizedDescription = savedDescription
                    try await manager.saveToPreferences()
                } else {
                    try await manager.removeFromPreferences()
                }
            }
            throw failure
        }
    }
}

import Darwin
import Foundation
import Network
import NetworkExtension

private func finish(_ flow: NEAppProxyFlow, _ error: Error?) {
    flow.closeReadWithError(error)
    flow.closeWriteWithError(error)
}

private func readFlow(_ flow: NEAppProxyTCPFlow) async throws -> Data? {
    try await withCheckedThrowingContinuation { continuation in
        flow.readData { data, error in
            if let error { continuation.resume(throwing: error) }
            else { continuation.resume(returning: data) }
        }
    }
}

private func tcp(_ flow: NEAppProxyTCPFlow, _ binding: Binding, _ pid: UInt32) async throws {
    let channel = Channel(socket: binding.socket)
    try await channel.open(flow.remoteFlowEndpoint, network: "Tcp", binding: binding, sourcePID: pid)
    try await flow.open(withLocalFlowEndpoint: nil)
    try await withThrowingTaskGroup(of: Void.self) { group in
        group.addTask {
            while let bytes = try await readFlow(flow), !bytes.isEmpty { try await channel.send(bytes) }
            try await channel.send(nil, final: true)
        }
        group.addTask {
            while let bytes = try await channel.read(32768) { try await flow.write(bytes) }
            flow.closeWriteWithError(nil)
        }
        do { while try await group.next() != nil {} }
        catch {
            channel.connection.cancel()
            finish(flow, error)
            throw error
        }
    }
}

private func udp(_ flow: NEAppProxyUDPFlow, _ binding: Binding, _ pid: UInt32) async throws {
    try await flow.open(withLocalFlowEndpoint: nil)
    var channels: [Network.NWEndpoint: Channel] = [:]
    defer {
        for channel in channels.values { channel.connection.cancel() }
    }
    while true {
        let (batch, error) = await flow.readDatagrams()
        if let error { throw error }
        guard let batch, !batch.isEmpty else { return }
        for (bytes, destination) in batch {
            let channel: Channel
            if let existing = channels[destination] {
                channel = existing
            } else {
                guard channels.count < 64 else { throw POSIXError(.ENOBUFS) }
                channel = Channel(socket: binding.socket)
                try await channel.open(destination, network: "Udp", binding: binding, sourcePID: pid)
                channels[destination] = channel
                Task {
                    do {
                        while true {
                            let header = try await channel.exact(2)
                            let length = Int(header[0]) << 8 | Int(header[1])
                            let reply = try await channel.exact(length)
                            try await flow.writeDatagrams([(reply, destination)])
                        }
                    } catch { finish(flow, error) }
                }
            }
            guard let size = UInt16(exactly: bytes.count) else { throw POSIXError(.EMSGSIZE) }
            var length = size.bigEndian
            try await channel.send(withUnsafeBytes(of: &length) { Data($0) } + bytes)
        }
    }
}

@objc(AstraProcessProvider)
final class ProcessProvider: NETransparentProxyProvider, NEAppProxyUDPFlowHandling {
    private let lock = NSLock()
    private var bindings: [String: Binding] = [:]
    private var active = 0

    private func replace(_ data: Data) throws {
        let next = try checkedBindings(data)
        lock.withLock { bindings = next }
    }

    override func startProxy(options: [String: Any]? = nil, completionHandler: @escaping (Error?) -> Void) {
        do {
            guard let config = (protocolConfiguration as? NETunnelProviderProtocol)?.providerConfiguration,
                  let rules = config["bindings"] as? Data else { throw POSIXError(.EINVAL) }
            try replace(rules)
        } catch { completionHandler(error); return }
        let settings = NETransparentProxyNetworkSettings(tunnelRemoteAddress: "127.0.0.1")
        settings.includedNetworkRules = [NENetworkRule(remoteNetworkEndpoint: nil, remotePrefix: 0,
            localNetworkEndpoint: nil, localPrefix: 0, protocol: .any, direction: .outbound)]
        setTunnelNetworkSettings(settings, completionHandler: completionHandler)
    }

    override func handleAppMessage(_ messageData: Data, completionHandler: ((Data?) -> Void)? = nil) {
        do { try replace(messageData); completionHandler?(Data([1])) }
        catch { completionHandler?(Data([0])) }
    }

    override func handleNewFlow(_ flow: NEAppProxyFlow) -> Bool {
        guard let data = flow.metaData.sourceAppAuditToken,
              data.count == MemoryLayout<audit_token_t>.size else { return false }
        var token = data.withUnsafeBytes { $0.loadUnaligned(as: audit_token_t.self) }
        let pid = audit_token_to_pid(token)
        guard pid > 0, pid != getpid() else { return false }
        var bytes = [CChar](repeating: 0, count: 4096)
        guard proc_pidpath_audittoken(&token, &bytes, UInt32(bytes.count)) > 0,
              let path = String(validatingCString: bytes),
              let rule = lock.withLock({ bindings[path] }) else { return false }
        let accepted = lock.withLock {
            guard rule.egress.kind != "block", active < 1024 else { return false }
            active += 1
            return true
        }
        guard accepted else { finish(flow, POSIXError(.EACCES)); return true }
        Task {
            defer { lock.withLock { active -= 1 } }
            do {
                if let stream = flow as? NEAppProxyTCPFlow { try await tcp(stream, rule, UInt32(pid)) }
                else if let stream = flow as? NEAppProxyUDPFlow { try await udp(stream, rule, UInt32(pid)) }
                else { throw POSIXError(.EPROTONOSUPPORT) }
                finish(flow, nil)
            } catch { finish(flow, error) }
        }
        return true
    }

    func handleNewUDPFlow(_ flow: NEAppProxyUDPFlow, initialRemoteFlowEndpoint: Network.NWEndpoint) -> Bool {
        handleNewFlow(flow)
    }
}

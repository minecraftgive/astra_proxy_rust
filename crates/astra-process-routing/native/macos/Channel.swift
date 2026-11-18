import Foundation
import Network

// Every destination gets an independent stream so UDP destinations on the
// same application socket retain their original addresses in both directions.
final class Channel {
    let connection: NWConnection
    init(socket: String) {
        connection = NWConnection(to: .unix(path: socket), using: .tcp)
        connection.stateUpdateHandler = { [weak connection] state in
            if case .waiting = state { connection?.cancel() }
        }
        connection.start(queue: DispatchQueue.global(qos: .userInitiated))
    }
    deinit { connection.cancel() }

    func send(_ data: Data?, final: Bool = false) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            connection.send(content: data, contentContext: final ? .finalMessage : .defaultMessage,
                            isComplete: true, completion: .contentProcessed { error in
                if let error { continuation.resume(throwing: error) }
                else { continuation.resume() }
            })
        }
    }

    func read(_ max: Int) async throws -> Data? {
        try await withCheckedThrowingContinuation { continuation in
            connection.receive(minimumIncompleteLength: 1, maximumLength: max) { data, _, _, error in
                if let error { continuation.resume(throwing: error) }
                else { continuation.resume(returning: data?.isEmpty == false ? data : nil) }
            }
        }
    }

    func exact(_ size: Int) async throws -> Data {
        var data = Data()
        while data.count < size {
            guard let chunk = try await read(size - data.count) else { throw POSIXError(.ECONNRESET) }
            data.append(chunk)
        }
        return data
    }

    func open(_ endpoint: Network.NWEndpoint, network: String, binding: Binding, sourcePID: UInt32) async throws {
        let timeout = DispatchWorkItem { [weak connection] in connection?.cancel() }
        DispatchQueue.global().asyncAfter(deadline: .now() + 10, execute: timeout)
        defer { timeout.cancel() }
        guard case let .hostPort(host, port) = endpoint else { throw POSIXError(.EAFNOSUPPORT) }
        let address: String
        switch host {
        case let .ipv4(ip): address = ip.debugDescription
        case let .ipv6(ip): address = ip.debugDescription
        case let .name(name, _): address = name
        @unknown default: throw POSIXError(.EAFNOSUPPORT)
        }
        struct Target: Encodable { let host: String; let port: UInt16 }
        struct Open: Encodable { let version = 1; let source_pid: UInt32; let target: Target; let network: String; let egress: Exit }
        let header = try JSONEncoder().encode(Open(source_pid: sourcePID, target: Target(host: address, port: port.rawValue),
                                                  network: network, egress: binding.egress))
        var size = UInt32(header.count).bigEndian
        try await send(withUnsafeBytes(of: &size) { Data($0) } + header)
        guard try await exact(1) == Data([1]) else { throw POSIXError(.ECONNREFUSED) }
    }
}

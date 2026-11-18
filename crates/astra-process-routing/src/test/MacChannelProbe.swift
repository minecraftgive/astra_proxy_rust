import Foundation
import Network

@main
struct Probe {
    static func main() async throws {
        let args = CommandLine.arguments
        let channel = Channel(socket: args[1])
        let exit = Exit(kind: args[5], tag: args[5] == "proxy" ? args[6] : nil,
                        name: args[5] == "interface" ? args[6] : nil)
        let binding = Binding(path: CommandLine.arguments[0], socket: args[1], egress: exit)
        let endpoint = NWEndpoint.hostPort(host: .init(args[3]), port: .init(rawValue: UInt16(args[4])!)!)
        do { try await channel.open(endpoint, network: args[2], binding: binding, sourcePID: UInt32(getpid())) }
        catch { exitWithFailure() }
        let bytes = Data("process-route-test".utf8)
        if args[2] == "Tcp" {
            try await channel.send(bytes)
            guard try await channel.exact(bytes.count) == bytes else { exitWithFailure() }
        } else {
            for body in exit.kind == "direct" ? [Data(), bytes] : [bytes] {
                var length = UInt16(body.count).bigEndian
                let frame = withUnsafeBytes(of: &length) { Data($0) } + body
                try await channel.send(frame)
                guard try await channel.exact(frame.count) == frame else { exitWithFailure() }
            }
        }
    }
    static func exitWithFailure() -> Never { Darwin.exit(2) }
}

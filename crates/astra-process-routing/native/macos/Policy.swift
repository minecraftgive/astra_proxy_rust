import Foundation

struct Exit: Codable {
    let kind: String
    let tag: String?
    let name: String?
}

struct Binding: Codable {
    let path: String
    let socket: String
    let egress: Exit
}

func checkedBindings(_ data: Data) throws -> [String: Binding] {
    guard data.count <= 1024 * 1024 else { throw POSIXError(.E2BIG) }
    let rules = try JSONDecoder().decode([Binding].self, from: data)
    var routes: [String: Binding] = [:]
    for rule in rules {
        guard rule.path.hasPrefix("/"), rule.socket.hasPrefix("/"), !rule.path.contains("\0"),
              !rule.socket.contains("\0"), rule.socket.utf8.count < 104 else { throw POSIXError(.EINVAL) }
        // Resolve aliases before comparing exact executable identities.
        let path = URL(fileURLWithPath: rule.path).standardizedFileURL.resolvingSymlinksInPath().path
        guard routes[path] == nil else { throw POSIXError(.EEXIST) }
        switch (rule.egress.kind, rule.egress.tag, rule.egress.name) {
        case ("block", nil, nil), ("direct", nil, nil): break
        case let ("interface", nil, name?) where !name.isEmpty: break
        case let ("proxy", tag?, nil) where !tag.isEmpty: break
        default: throw POSIXError(.EINVAL)
        }
        routes[path] = rule
    }
    return routes
}

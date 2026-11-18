import Foundation
import NetworkExtension

@main
struct ExtensionMain {
    static func main() {
        NEProvider.startSystemExtensionMode()
        dispatchMain()
    }
}

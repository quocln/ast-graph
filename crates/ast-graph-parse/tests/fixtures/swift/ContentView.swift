import SwiftUI

/// Main content view for the app.
public struct ContentView: View {
    @State private var count: Int = 0
    @State private var title: String = "Counter"

    public var body: some View {
        VStack {
            Text(headerText)
            Button("Increment") {
                increment()
            }
            Button("Reset") {
                reset()
            }
        }
    }

    /// Computed header combining title and count.
    var headerText: String {
        formatHeader(title: title, count: count)
    }

    private func increment() {
        count += 1
        logEvent("increment")
    }

    private func reset() {
        count = 0
        logEvent("reset")
    }

    private func logEvent(_ name: String) {
        Logger.log(name)
    }
}

private func formatHeader(title: String, count: Int) -> String {
    return "\(title): \(count)"
}

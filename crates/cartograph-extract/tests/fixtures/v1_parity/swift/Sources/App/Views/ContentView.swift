import SwiftUI

@main
struct ShopApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}

struct ContentView: View {
    @State private var count = 0

    var body: some View {
        VStack {
            ProfileView(user: User.make(named: "a"))
            Button("Tap") { increment() }
        }
    }

    private func increment() {
        count += 1
    }
}

struct ProfileView: View, Equatable {
    let user: User

    var body: some View {
        Text(user.displayName())
    }
}

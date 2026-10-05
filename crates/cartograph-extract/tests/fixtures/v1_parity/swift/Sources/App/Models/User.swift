import Foundation
@preconcurrency import Security

public typealias UserID = UUID

let defaultRegion = "eu"
var activeSessions: Int = 0

public protocol Identifiable {
    associatedtype ID
    var id: ID { get }
}

public protocol Repository: Identifiable {
    func find(id: String) async throws -> User?
    func save(_ user: User) async throws
}

public enum Role: String, Codable {
    case admin
    case member, guest
}

public struct Address: Equatable {
    let street: String
    var city: String
    var label: String { "\(street), \(city)" }
}

open class Entity {
    public let created: Date
    public init(created: Date = Date()) {
        self.created = created
    }
}

public final class User: Entity, Identifiable, Codable {
    public let id: UserID
    public var name: String
    private var role: Role = .member
    fileprivate var address: Address?
    public static let anonymous = User(name: "anon")

    public init(name: String) {
        self.id = UUID()
        self.name = name
        super.init()
    }

    public func displayName() -> String {
        return name.capitalized
    }

    func promote(to newRole: Role) {
        role = newRole
        activeSessions += 1
    }

    class func make(named name: String) -> User {
        return User(name: name)
    }

    static func validate(_ name: String) -> Bool {
        return !name.isEmpty
    }

    @objc func refresh() async {
        _ = displayName()
    }

    func fetch(cfg: Config) -> [Address] {
        return []
    }

    deinit {}
}

public struct Config {
    let retries: Int
}

extension User: CustomStringConvertible {
    public var description: String { displayName() }

    func greet() -> String {
        return "hi " + displayName()
    }
}

func overload(_ value: Int) -> Int { value }
func overload(_ value: String) -> String { value }

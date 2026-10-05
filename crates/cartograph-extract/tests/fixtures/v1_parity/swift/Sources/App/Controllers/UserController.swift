import Vapor

struct UserController: RouteCollection {
    func boot(routes: RoutesBuilder) throws {
        let users = routes.grouped("users")
        users.get(use: index)
        users.post(use: create)
    }

    func index(req: Request) async throws -> [String] {
        return [User.anonymous.displayName()]
    }

    func create(req: Request) async throws -> String {
        let user = User(name: "new")
        return user.greet()
    }
}

struct AuthMiddleware: AsyncMiddleware {
    func respond(to request: Request, chainingTo next: AsyncResponder) async throws -> Response {
        return try await next.respond(to: request)
    }
}

func routes(_ app: Application) throws {
    app.get("health") { req in "ok" }
    app.post("orders") { req in "created" }
    app.grouped("api").get("users") { req in "list" }
    try app.register(collection: UserController())
}

import Vapor

final class LoggingMiddleware: Middleware {
    func respond(to request: Request, chainingTo next: Responder) -> EventLoopFuture<Response> {
        request.logger.info("\(request.url)")
        return next.respond(to: request)
    }
}

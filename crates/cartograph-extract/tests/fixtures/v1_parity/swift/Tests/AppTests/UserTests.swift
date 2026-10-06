import XCTest
@testable import App

final class UserTests: XCTestCase {
    func testDisplayName() {
        let user = User(name: "ada")
        XCTAssertEqual(user.displayName(), "Ada")
    }

    func testValidate() {
        XCTAssertTrue(User.validate("x"))
    }
}

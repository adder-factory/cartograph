import UIKit

protocol ProfileDelegate: AnyObject {
    func profileDidUpdate(_ user: User)
}

protocol ProfileDataSource {
    func numberOfUsers() -> Int
}

class UserCell: UITableViewCell {
    func configure(with user: User) {
        textLabel?.text = user.displayName()
    }
}

class BadgeView: UIView {
    var count: Int = 0
}

class ProfileViewController: UIViewController, ProfileDelegate {
    weak var delegate: ProfileDelegate?
    private let badge = BadgeView()

    override func viewDidLoad() {
        super.viewDidLoad()
        view.addSubview(badge)
        delegate?.profileDidUpdate(User.make(named: "b"))
    }

    func profileDidUpdate(_ user: User) {
        badge.count += 1
    }
}

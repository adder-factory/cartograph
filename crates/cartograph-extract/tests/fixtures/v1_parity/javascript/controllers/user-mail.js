class UserController {
  getUser(req, res) {
    res.json({ id: req.params.id });
  }
}

class MailService {
  send(message) {
    return deliver(message);
  }
}

function deliver(message) {
  return message;
}

module.exports = { UserController, MailService, deliver };

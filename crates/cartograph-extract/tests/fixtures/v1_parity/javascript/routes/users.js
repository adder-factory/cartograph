const router = require('../lib/router');
const app = require('../app');
const { auth } = require('../middleware/auth');
const UserController = require('../controllers/user-mail');
const MailService = require('../controllers/user-mail');

function listUsers(req, res) {
  res.json([]);
}

function login(req, res) {
  MailService.send(req.body);
}

function serveStatic() {}

router.get('/users', listUsers);
router.get('/users/:id', authMiddleware, UserController.getUser);
app.post('/login', login);
app.use('/static', serveStatic);
app.all('/ping', (req, res) => res.end());

function wire(req, res) {
  authMiddleware(req, res);
  UserController.getUser(req, res);
  MailHelper.send(req.body);
  validatebody(req);
}

module.exports = { router, wire };

function auth(req, res, next) {
  next();
}

function ValidateBody(req, res, next) {
  next();
}

module.exports = { auth, ValidateBody };

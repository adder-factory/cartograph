import app.models as m
import app.sub.deep as d
from app.models import User, Repo, make_user
from app.utils import run
from app import utils
from .models import Base, registry
from . import utils as u
from . import helper
from .sub import initf
from .sub import deep
from typing import List


class Local:
    pass


class UserService(Base):
    repo: Repo

    def __init__(self, repo: Repo):
        super().__init__("service")
        self.repo = repo

    def create(self, name: str) -> User:
        user = make_user(name)
        other = m.make_user(name)
        self.repo.save(user)
        self.repo.save(other)
        self.notify(user)
        return user

    def notify(self, user: User) -> None:
        run({"user": user.name})
        utils.run({})
        u._private_run()
        helper()
        initf()
        deep.deep()
        d.deep()

    def batch(self, names: List[str]) -> List[User]:
        x: Local = Local()
        registry.register(x)
        return [self.create(n) for n in names]


def main():
    service = UserService(Repo())
    service.create("ada")
    User.anonymous()
    return service

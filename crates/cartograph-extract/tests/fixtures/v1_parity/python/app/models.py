import abc
import typing
from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Protocol, Generic, TypeVar, Optional

T = TypeVar("T")
MAX_SIZE = 10
DEFAULT_NAME = "anon"
config = {"debug": False}


class Registry:
    def register(self, item):
        return item


registry: Registry = Registry()


class Renderer(Protocol):
    def render(self) -> str: ...


class Drawable(typing.Protocol):
    def draw(self) -> None: ...


class Shape(ABC):
    @abstractmethod
    def area(self) -> float: ...


class Solid(abc.ABC):
    pass


class MyProtocol:
    pass


class Base:
    LIMIT = 5

    def __init__(self, name: str):
        self.name = name

    def describe(self) -> str:
        return self.name


class Box(Generic[T]):
    item: T


@dataclass
class User(Base, MyProtocol):
    email: str
    repo: "Repo" = None

    def __init__(self, name: str, email: str):
        super().__init__(name)
        self.email = email

    @staticmethod
    def build(name: str) -> "User":
        return User(name, "")

    @classmethod
    def anonymous(cls):
        return cls.build(DEFAULT_NAME)

    @property
    def label(self) -> str:
        return self.describe()

    async def refresh(self) -> None:
        await self.load()

    async def load(self):
        return None


class Repo:
    def __init__(self):
        self.items = []

    def save(self, user: User) -> User:
        self.items.append(user)
        self.validate(user)
        return user

    def find(self, name: str) -> Optional[User]:
        return None

    def validate(self, user: User) -> bool:
        return bool(user.email)


class Circle(Shape, Renderer):
    def area(self) -> float:
        return 3.14

    def render(self) -> str:
        return "circle"


def make_user(name: str) -> User:
    return User.build(name)

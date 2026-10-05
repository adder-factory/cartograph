from flask import Flask, Blueprint
from fastapi import APIRouter, FastAPI
from app.services import UserService
from app.models import Repo

app = Flask(__name__)
bp = Blueprint("users", __name__)
router = APIRouter()
api = FastAPI()


@app.route("/home")
def home():
    return "home"


@bp.route("/users")
def list_users():
    service = UserService(Repo())
    return service.batch(["a"])


@bp.route("/u2", methods=["POST"])
def create_user():
    return "created"


@router.get("/items")
def get_items():
    return []


@api.post("/items")
async def create_item():
    return {}


@router.delete("/items/{item_id}")
def delete_item(item_id: int):
    return item_id

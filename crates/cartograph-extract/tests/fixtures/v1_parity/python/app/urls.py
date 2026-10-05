from django.urls import path, re_path
from app import views

urlpatterns = [
    path("dashboard/", views.home, name="dashboard"),
    path("users/<int:pk>/", views.list_users, name="user-detail"),
]

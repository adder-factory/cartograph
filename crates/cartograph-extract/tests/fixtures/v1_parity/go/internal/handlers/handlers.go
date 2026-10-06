package handlers

import (
	"net/http"

	"example.com/shop/internal/service"
)

type Router interface {
	GET(path string, h http.HandlerFunc)
	POST(path string, h http.HandlerFunc)
}

type UserHandler struct {
	svc *service.UserService
}

func (h *UserHandler) HandleCreate(w http.ResponseWriter, r *http.Request) {
	_, _ = h.svc.Register(r.FormValue("email"))
}

func HandleList(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
}

// register wires routes on an abstract router (no framework import).
func register(r Router, h *UserHandler) {
	r.GET("/users", HandleList)
	r. POST ("/users", h.HandleCreate)
}

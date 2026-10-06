package models

import (
	"io"
	"sync"
)

// MaxUsers bounds the in-memory store.
const MaxUsers = 100

const (
	RoleAdmin = iota
	RoleMember
	roleGuest
)

var (
	DefaultRegion = "eu"
	retryBudget   = 3
)

var Single = 99

// ID is a type alias style declaration.
type ID string

type Labels = map[string]string

type Base struct {
	Created int64
}

type Auditable interface {
	Audit() string
}

type Named interface {
	io.Writer
	Auditable
	Name() string
}

// User embeds Base and a mutex.
type User struct {
	Base
	*sync.Mutex
	ID       ID
	Email    string `json:"email"`
	age      int
	X, Y     float64
	Profile  *Profile
	Tags     Labels
}

type Profile struct {
	Bio string
}

func (u *User) Name() string {
	return u.Email
}

func (u User) Audit() string {
	return string(u.ID)
}

func (u *User) Write(p []byte) (int, error) {
	return len(p), nil
}

// NewUser builds a user with composite literals.
func NewUser(email string) *User {
	p := &Profile{Bio: "new"}
	u := &User{Email: email, Profile: p}
	if len(u.Email) > MaxUsers {
		return nil
	}
	return u
}

func Limit() int {
	return MaxUsers
}

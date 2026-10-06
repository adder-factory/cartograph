package service

import (
	"fmt"
	m "example.com/shop/internal/models"
	. "strings"
	_ "embed"
)

type Store interface {
	Save(u *m.User) error
	Find(id m.ID) (*m.User, error)
}

type UserService struct {
	store Store
	cache map[m.ID]*m.User
}

type Committer struct{}

func (c *Committer) Commit() error { return nil }

type Builder struct{}

func (b *Builder) Build() *Committer { return &Committer{} }

func NewUserService(s Store) *UserService {
	return &UserService{store: s, cache: map[m.ID]*m.User{}}
}

func (s *UserService) Register(email string) (*m.User, error) {
	var local m.Profile
	_ = local
	u := m.NewUser(ToLower(email))
	if err := s.store.Save(u); err != nil {
		return nil, fmt.Errorf("save: %w", err)
	}
	s.audit(u)
	b := &Builder{}
	_ = b.Build().Commit()
	return u, nil
}

func (s *UserService) audit(u *m.User) {
	fmt.Println(u.Audit(), m.Limit(), m.DefaultRegion)
}

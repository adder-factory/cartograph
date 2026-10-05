package models

import "testing"

func TestNewUser(t *testing.T) {
	u := NewUser("a@example.invalid")
	if u.Name() == "" {
		t.Fatal("empty")
	}
}

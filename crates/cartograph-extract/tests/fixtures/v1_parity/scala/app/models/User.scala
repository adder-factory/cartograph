package models

import java.time.Instant

type UserId = Long

val DefaultName = "anon"
var created = 0

trait Named {
  def displayName: String
}

trait Audited extends Named {
  def audit(): String = s"audit:$displayName"
}

sealed abstract class Entity(val id: UserId)

case class User(override val id: UserId, name: String) extends Entity(id) with Audited {
  val createdAt: Instant = Instant.now()
  var email: Option[String] = None

  def displayName: String = name

  def rename(newName: String): User = copy(name = newName)
}

object User {
  def create(id: UserId): User = {
    created += 1
    new User(id, DefaultName)
  }
}

enum Role {
  case Admin, Member
  case Guest
}

extension (u: User) {
  def initials: String = u.name.take(2)
}

class UserBuilder {
  private var current: UserId = 0L

  def withId(id: UserId): UserBuilder = {
    current = id
    this
  }

  def build(): Committer = new Committer(User.create(current))
}

class Committer(user: User) {
  def commit(): User = user
}

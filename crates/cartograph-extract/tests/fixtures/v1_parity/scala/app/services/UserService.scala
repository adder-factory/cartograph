package services

import models.{User, UserBuilder}
import models.{Role => UserRole}
import scala.collection.mutable._
import javax.inject.Inject

class UserRepository {
  private val store = HashMap.empty[Long, User]

  def find(id: Long): Option[User] = store.get(id)

  def save(user: User): Unit = store.put(user.id, user)
}

class UserService @Inject() (repo: UserRepository) {
  def load(id: Long): User = {
    val user = repo.find(id).getOrElse(User.create(id))
    helper(user)
    this.track(user)
    user
  }

  def build(id: Long): User = new UserBuilder().withId(id).build().commit()

  private def helper(user: User): Unit = repo.save(user)

  private def track(user: User): Unit = println(user.initials)

  def roleOf(user: User): UserRole = UserRole.Member
}

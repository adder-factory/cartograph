package com.acme.app.model

import java.time.Instant

typealias UserId = String

const val MAX_USERS = 100
val defaultRole = Role.USER
var userCounter = 0

enum class Role {
    ADMIN,
    USER;

    fun label(): String = name.lowercase()
}

interface Named {
    val displayName: String
}

interface Auditable : Named {
    fun audit(): String
}

fun interface Validator {
    fun validate(user: User): Boolean
}

abstract class Entity(open val id: UserId)

data class User(override val id: UserId, var name: String, val role: Role = Role.USER) : Entity(id), Auditable {
    private val createdAt: Instant = Instant.now()
    var email: String? = null

    override val displayName: String
        get() = name

    override fun audit(): String = "$id:$name"

    fun rename(newName: String): User {
        name = newName
        return this
    }

    companion object {
        fun create(id: UserId): User = User(id, "anon")
    }
}

object Registry {
    private val users = mutableListOf<User>()

    fun register(user: User) {
        users.add(user)
        userCounter += 1
    }
}

fun String.shout(): String = this.uppercase()

class UserBuilder {
    private var id: UserId = ""

    fun withId(id: UserId): UserBuilder {
        this.id = id
        return this
    }

    fun build(): Committer = Committer(User.create(id))
}

class Committer(private val user: User) {
    fun commit(): User = user
}

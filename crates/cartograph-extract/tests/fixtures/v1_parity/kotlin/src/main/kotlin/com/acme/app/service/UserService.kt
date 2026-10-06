package com.acme.app.service

import com.acme.app.b.Engine
import com.acme.app.model.User
import com.acme.app.model.UserBuilder
import com.acme.app.model.Registry
import com.acme.app.model.shout
import com.acme.app.model.Validator as UserValidator
import org.springframework.stereotype.Service
import org.springframework.transaction.annotation.Transactional
import java.util.*

@Service
class UserService(private val engine: Engine) {
    private val validator: UserValidator = UserValidator { it.name.isNotEmpty() }
    private val cache: MutableMap<String, User> = HashMap()

    @Transactional
    fun load(id: String): User {
        val user = User.create(id)
        if (!validator.validate(user)) {
            throw IllegalStateException(id.shout())
        }
        Registry.register(user)
        engine.run()
        helper(user)
        this.track(user)
        return user
    }

    fun build(id: String): User {
        val builder = UserBuilder()
        return builder.withId(id).build().commit()
    }

    fun external(value: String): String = JHelper.go(value)

    private fun helper(user: User) {
        cache[user.id] = user
    }

    private fun track(user: User) {
        user.rename(user.name)
    }
}

class KTagger {
    fun tag(value: String): String = "#" + value
}

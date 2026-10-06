package com.acme.app.web

import com.acme.app.model.User
import com.acme.app.service.UserService
import org.springframework.beans.factory.annotation.Value
import org.springframework.web.bind.annotation.GetMapping
import org.springframework.web.bind.annotation.PathVariable
import org.springframework.web.bind.annotation.PostMapping
import org.springframework.web.bind.annotation.RequestMapping
import org.springframework.web.bind.annotation.RestController

@RestController
@RequestMapping("/users")
class UserController(private val userService: UserService) {
    @Value("\${app.greeting}")
    lateinit var greeting: String

    @GetMapping("/{id}")
    fun show(@PathVariable id: String): User = userService.load(id)

    @PostMapping("/build")
    fun build(@PathVariable("id") id: String): User {
        return userService.build(id)
    }
}

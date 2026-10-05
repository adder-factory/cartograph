package com.acme.config

import org.springframework.beans.factory.annotation.Value
import org.springframework.stereotype.Component

@Component
class GreetingConfig {
    @Value("\${greeting.hello}")
    lateinit var hello: String

    @Value("#{'\${greeting.bye}'}")
    lateinit var bye: String
}

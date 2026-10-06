package com.acme.views

import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition

class SettingsHelper {
    fun run() {}
}

class SettingsModule : Module() {
    override fun definition() = ModuleDefinition {
        Name("ExpoSettings")
        Constants("PI" to 3.14)
        Function("getTheme") { "dark" }
        AsyncFunction("setTheme") { theme: String -> theme.uppercase() }
        Function ("spaced") { 1 }
        Property("version") { "1.0" }
    }
}

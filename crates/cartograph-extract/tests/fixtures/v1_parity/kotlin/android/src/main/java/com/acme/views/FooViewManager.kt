package com.acme.views

import android.view.View
import com.facebook.react.uimanager.SimpleViewManager
import com.facebook.react.uimanager.annotations.ReactProp

class FooViewManager : SimpleViewManager<View>() {
    override fun getName() = "Foo"

    @ReactProp(name = "color")
    fun setColor(view: View, color: Int) {}

    @ReactProp(name = "enabled")
    fun setEnabled(view: View, enabled: Boolean) {}
}

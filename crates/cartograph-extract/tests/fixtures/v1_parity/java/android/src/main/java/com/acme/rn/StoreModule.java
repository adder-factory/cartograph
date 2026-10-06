package com.acme.rn;

import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.ReactContextBaseJavaModule;
import com.facebook.react.bridge.ReactMethod;

public class StoreModule extends ReactContextBaseJavaModule {
    StoreModule(ReactApplicationContext context) { super(context); }

    @Override
    public String getName() { return "Store"; }

    @ReactMethod
    public void save(String key) {
        persist(key);
    }

    @ReactMethod(isBlockingSynchronousMethod = true)
    public String syncGet() { return "value"; }

    private void persist(String key) {}
}

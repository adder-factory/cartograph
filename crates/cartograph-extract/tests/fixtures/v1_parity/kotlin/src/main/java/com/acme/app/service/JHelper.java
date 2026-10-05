package com.acme.app.service;

public class JHelper {
    public static String go(String value) { return value.trim(); }

    public String tag(String value) {
        return new KTagger().tag(value);
    }
}

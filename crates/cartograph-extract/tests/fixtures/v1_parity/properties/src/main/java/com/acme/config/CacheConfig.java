package com.acme.config;

import org.springframework.beans.factory.annotation.Value;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.context.annotation.Configuration;

@Configuration
@ConditionalOnProperty(prefix = "feature.payments", name = "enabled", havingValue = "true")
public class CacheConfig {
    @Value("${app.cache.ttl}")
    private int cacheTtl;

    @Value("${app.name:Default}")
    private String appName;

    @Value("${orders.page-size}-${app.indented}")
    private String combined;

    @Value("${missing.key}")
    private String missing;

    @ConditionalOnProperty("feature.payments.url")
    public String paymentsUrl() {
        return appName;
    }
}

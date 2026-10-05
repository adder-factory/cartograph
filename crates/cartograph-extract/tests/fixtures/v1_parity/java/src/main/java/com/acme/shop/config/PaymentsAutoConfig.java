package com.acme.shop.config;

import org.springframework.beans.factory.annotation.Value;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

@Configuration
@ConditionalOnProperty(prefix = "feature.payments", name = "enabled", havingValue = "true")
public class PaymentsAutoConfig {
    @Value("${feature.payments.url:fallback}")
    private String url;

    @Value("page-${orders.page-size}")
    private String pageLabel;

    @Bean
    public PaymentsClient paymentsClient() {
        return new PaymentsClient(url);
    }
}

class PaymentsClient {
    private final String endpoint;

    PaymentsClient(String endpoint) { this.endpoint = endpoint; }
}

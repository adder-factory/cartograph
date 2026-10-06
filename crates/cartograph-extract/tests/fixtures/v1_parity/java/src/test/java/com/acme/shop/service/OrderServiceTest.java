package com.acme.shop.service;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertEquals;

class OrderServiceTest {
    @Test
    void namesTheService() {
        OrderService service = new OrderService(null);
        assertEquals("orders", service.name());
    }
}

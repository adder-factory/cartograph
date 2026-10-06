package com.acme.shop.web;

import com.acme.shop.model.Order;
import com.acme.shop.service.OrderService;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.web.bind.annotation.*;
import static java.util.Objects.requireNonNull;

@RestController
@RequestMapping("/api/orders")
public class OrderController {
    @Value("${app.cache.ttl}")
    private int cacheTtl;

    private final OrderService orderService;

    public OrderController(OrderService orderService) {
        this.orderService = requireNonNull(orderService);
    }

    @GetMapping("/{id}")
    public Order show(@PathVariable("id") String id) {
        return orderService.find(id);
    }

    @PostMapping(value = "/create")
    public Order create(@RequestBody String id) {
        Order order = new Order(id);
        order.addLine("first");
        return order;
    }

    @GetMapping("/legacy")
    @PostMapping("/legacy-post")
    public String legacy() {
        return describe();
    }

    @DeleteMapping(path = "/{id}")
    public void remove(@PathVariable String id) {
        this.describe();
    }

    private String describe() {
        return "ttl=" + cacheTtl;
    }
}

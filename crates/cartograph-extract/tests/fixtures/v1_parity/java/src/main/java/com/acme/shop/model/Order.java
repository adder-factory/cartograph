package com.acme.shop.model;

import java.util.List;
import java.util.ArrayList;

public class Order {
    public static final int MAX_LINES = 50;
    private final String id;
    private OrderStatus status;
    private List<String> lines = new ArrayList<>();

    public Order(String id) {
        this.id = id;
        this.status = OrderStatus.NEW;
    }

    public String getId() { return id; }

    public OrderStatus getStatus() { return status; }

    public void addLine(String sku) {
        lines.add(sku);
        validate();
    }

    private void validate() {
        if (lines.size() > MAX_LINES) {
            throw new IllegalStateException("too many lines");
        }
    }
}

enum OrderStatus {
    NEW,
    PAID("paid"),
    SHIPPED("shipped");

    private final String label;

    OrderStatus() { this("new"); }

    OrderStatus(String label) { this.label = label; }

    public String label() { return label; }
}

class OrderBuilder {
    private String id;

    public static OrderBuilder create() { return new OrderBuilder(); }

    public OrderBuilder withId(String id) {
        this.id = id;
        return this;
    }

    public Committer build() { return new Committer(new Order(id)); }
}

class Committer {
    private final Order order;

    Committer(Order order) { this.order = order; }

    public Order commit() { return order; }
}

class OrderFactory {
    public Order make(String id) {
        OrderBuilder builder = OrderBuilder.create();
        return builder.withId(id).build().commit();
    }

    public Order direct(Order seed) {
        Committer committer = new Committer(seed);
        return committer.commit();
    }
}

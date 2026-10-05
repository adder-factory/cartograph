package com.acme.shop.service;

import com.acme.shop.model.Order;
import com.acme.shop.service.converter.FooConverter;
import org.springframework.stereotype.Service;
import org.springframework.transaction.annotation.Transactional;
import java.util.*;

interface Port {
    String name();
}

interface OrderPort extends Port {
    Order find(String id);
}

abstract class BaseService<T> {
    protected abstract T load(String id);
}

@Service
public class OrderService extends BaseService<Order> implements OrderPort, Comparable<OrderService> {
    private final OrderRepository orderRepository;
    private FooConverter fooConverter;
    private Map<String, Order> cache = new HashMap<>();

    public OrderService(OrderRepository orderRepository) {
        this.orderRepository = orderRepository;
    }

    @Override
    public String name() { return "orders"; }

    @Override
    @Transactional(readOnly = true)
    public Order find(String id) {
        String key = fooConverter.convert(id);
        Order cached = cache.get(key);
        if (cached != null) {
            return cached;
        }
        return load(key);
    }

    @Override
    protected Order load(String id) {
        Order order = orderRepository.findById(id);
        audit(order);
        this.track(order);
        return order;
    }

    private void audit(Order order) {
        Helper.log(order.getId());
    }

    private void track(Order order) {
        order.addLine("audit");
    }

    @Override
    public int compareTo(OrderService other) { return 0; }
}

interface OrderRepository {
    Order findById(String id);
}

class Helper {
    static void log(String message) {
        System.out.println(message);
    }
}

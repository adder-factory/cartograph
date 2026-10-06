using System;
using Shop.Models;
using Xunit;

namespace Shop.Tests;

public class OrderTests
{
    [Fact]
    public void AddLineKeepsStatus()
    {
        var order = new Order(Guid.Empty);
        order.AddLine("x");
        Assert.Equal(OrderStatus.New, order.Status);
    }
}

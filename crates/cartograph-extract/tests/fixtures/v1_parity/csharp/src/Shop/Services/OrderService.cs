using System;
using System.Threading.Tasks;
using Shop.Data;
using Shop.Models;
using static System.Math;
using Clock = System.DateTime;

namespace Shop.Services;

public interface IOrderService
{
    Task<Order> FindAsync(Guid id);
}

public abstract class ServiceBase<T> where T : IEntity
{
    protected abstract T Load(Guid id);
}

public sealed class OrderService : ServiceBase<Order>, IOrderService
{
    private readonly IOrderRepository _repository;
    private readonly OrderBuilder _builder = new OrderBuilder();

    public OrderService(IOrderRepository repository)
    {
        _repository = repository;
    }

    public async Task<Order> FindAsync(Guid id)
    {
        var order = await _repository.GetAsync(id);
        Log(order);
        return order ?? Load(id);
    }

    protected override Order Load(Guid id)
    {
        var order = Order.Create();
        order.AddLine("default");
        return order;
    }

    public Order Build(Guid id) => _builder.WithId(id).Build().Commit();

    public void Import((Order order, int count) batch, Money price)
    {
        Console.WriteLine(Max(batch.count, 1));
        Console.WriteLine(Clock.UtcNow);
    }

    private static void Log(Order order) => Console.WriteLine(order.Audit());
}

public class OrderBuilder
{
    private Guid _id;

    public OrderBuilder WithId(Guid id)
    {
        _id = id;
        return this;
    }

    public Committer Build() => new Committer(new Order(_id));
}

public class Committer
{
    private readonly Order _order;

    public Committer(Order order) { _order = order; }

    public Order Commit() => _order;
}

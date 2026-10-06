using System;
using System.Threading.Tasks;
using Shop.Models;

namespace Shop.Data
{
    public interface IOrderRepository
    {
        Task<Order> GetAsync(Guid id);
    }

    internal class OrderRepository : IOrderRepository
    {
        public Task<Order> GetAsync(Guid id) => Task.FromResult(new Order(id));
    }
}

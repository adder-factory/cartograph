using System;
using System.Collections.Generic;

namespace Shop.Models
{
    public enum OrderStatus
    {
        New,
        Paid = 2,
        Shipped
    }

    public interface IEntity
    {
        Guid Id { get; }
    }

    public interface IAuditable : IEntity
    {
        string Audit();
    }

    public struct Money
    {
        public decimal Amount;

        public Money(decimal amount) { Amount = amount; }

        public static Money Zero() => new Money(0m);
    }

    public class Order : IAuditable
    {
        public const int MaxLines = 50;
        private readonly List<string> _lines = new List<string>();

        public Guid Id { get; private set; }
        public OrderStatus Status { get; set; } = OrderStatus.New;
        public Money Total { get; set; }

        public Order(Guid id)
        {
            Id = id;
        }

        public string Audit() => $"{Id}:{Status}";

        public void AddLine(string sku)
        {
            _lines.Add(sku);
            Validate();
            this.Touch();
        }

        private void Validate()
        {
            if (_lines.Count > MaxLines) throw new InvalidOperationException("too many");
        }

        private void Touch() { }

        public static Order Create() => new Order(Guid.NewGuid());
    }

    public record OrderSummary(Guid Id, decimal Total);

    public class Customer(string name, int tier)
    {
        public string Name { get; } = name;
        public int Tier => tier;
    }
}

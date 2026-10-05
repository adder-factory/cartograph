using System;
using System.Threading.Tasks;
using Microsoft.AspNetCore.Mvc;
using Shop.Models;
using Shop.Services;

namespace Shop.Controllers
{
    [ApiController]
    [Route("api/[controller]")]
    public class OrdersController : ControllerBase
    {
        private readonly IOrderService _orders;

        public OrdersController(IOrderService orders)
        {
            _orders = orders;
        }

        [HttpGet("{id}")]
        public async Task<ActionResult<Order>> Get(Guid id)
        {
            var order = await _orders.FindAsync(id);
            return Ok(order);
        }

        [HttpPost]
        public IActionResult Create([FromBody] OrderSummary summary) => Created("", summary);

        [HttpGet("/a")]
        [HttpPost("/b")]
        public IActionResult Both() => Ok();
    }

    public class LegacyController : ControllerBase
    {
        [HttpGet("/api/legacy")]
        public IActionResult List() => Ok();

        [HttpDelete ( "/api/legacy/{id}" )]
        public IActionResult Remove(int id) => NoContent();
    }
}

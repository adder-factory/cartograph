using Shop.Data;
using Shop.Services;

var builder = WebApplication.CreateBuilder(args);
builder.Services.AddScoped<IOrderService, OrderService>();
var app = builder.Build();

app.MapGet("/health", () => "ok");
app.MapGet ("/ready", () => "ready");
app.MapPost("/orders", (OrderService svc) => svc.Build(System.Guid.NewGuid()));
app.MapPut("/orders/{id}", (int id) => id);
app.MapDelete("/orders/{id}", (int id) => id);
app.Run();

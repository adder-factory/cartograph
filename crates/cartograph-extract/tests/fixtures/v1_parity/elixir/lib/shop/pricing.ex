defmodule Shop.Pricing do
  def price(%{price: p, qty: q}), do: p * q

  def format(amount) do
    :erlang.float_to_binary(amount, decimals: 2)
  end

  def tax(amount), do: round_cents(amount * 0.2)

  defp round_cents(value), do: Float.round(value, 2)
end

defprotocol Shop.Priceable do
  def price_of(item)
end

defimpl Shop.Priceable, for: Map do
  def price_of(item), do: Shop.Pricing.price(item)
end

defmodule Shop.Errors.InvalidItem do
  defexception message: "invalid item"
end

defmodule Shop.CartTest do
  use ExUnit.Case
  alias Shop.Cart

  test "adds an item" do
    cart = Cart.new("ada")
    {:ok, cart} = Cart.add(cart, %{price: 2, qty: 3})
    assert Cart.total(cart) == 6
    helper(cart)
  end

  defp helper(cart), do: Shop.Pricing.tax(Cart.total(cart))
end

defmodule Shop.Cart do
  @moduledoc "Shopping cart."
  alias Shop.Pricing
  import Enum, only: [map: 2]
  require Logger

  defstruct items: [], owner: nil

  @max_items 50

  def new(owner), do: %__MODULE__{owner: owner}

  def add(%__MODULE__{} = cart, item) when is_map(item) do
    if length(cart.items) >= @max_items do
      {:error, :full}
    else
      {:ok, %{cart | items: [item | cart.items]}}
    end
  end

  def total(cart) do
    cart.items
    |> map(&Pricing.price/1)
    |> Enum.sum()
    |> apply_discount()
  end

  defp apply_discount(amount) when amount > 100, do: amount * 0.9
  defp apply_discount(amount), do: amount

  def empty?, do: false

  defmacro debug(msg) do
    quote do
      Logger.debug(unquote(msg))
    end
  end

  defdelegate format(amount), to: Shop.Pricing

  defguard is_positive(x) when is_number(x) and x > 0
end
